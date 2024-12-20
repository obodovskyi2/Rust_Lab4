use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{self, protocol::Message as WsMessage},
};

#[derive(Clone, Serialize, Deserialize)]
struct Message {
    from: String,
    content: String,
    timestamp: i64,
}

struct ChatServer {
    messages: Arc<Mutex<Vec<Message>>>,
    users: Arc<Mutex<HashMap<String, String>>>,
    connected_users: Arc<Mutex<HashSet<String>>>,
    tx: broadcast::Sender<Message>,
}

impl ChatServer {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(100);
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            users: Arc::new(Mutex::new(HashMap::new())),
            connected_users: Arc::new(Mutex::new(HashSet::new())),
            tx,
        }
    }

    async fn register_user(&self, username: &str, password: &str) -> Result<(), String> {
        let mut users = self.users.lock().await;
        if username.is_empty() || password.is_empty() {
            return Err("Username or password cannot be empty.".into());
        }
        if users.contains_key(username) {
            return Err("Username already exists".into());
        }
        users.insert(username.to_string(), password.to_string());
        Ok(())
    }

    async fn authenticate_user(&self, username: &str, password: &str) -> bool {
        let users = self.users.lock().await;
        match users.get(username) {
            Some(stored) => stored == password,
            None => false,
        }
    }

    async fn broadcast_message(&self, message: Message) {
        if let Err(e) = self.tx.send(message.clone()) {
            eprintln!("Broadcast error: {}", e);
        }
        self.messages.lock().await.push(message);
    }

    async fn handle_connection(&self, stream: TcpStream) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("WebSocket handshake failed: {}", e);
                return;
            }
        };

        println!("Client connected.");
        let (mut write, mut read) = ws_stream.split();

        // Subscription to broadcast channel
        let mut rx = self.tx.subscribe();

        // ---- STEP 1: AUTHENTICATION ----
        let initial_msg = match read.next().await {
            Some(Ok(WsMessage::Text(msg))) => msg,
            Some(Ok(WsMessage::Binary(_))) => {
                let _ = write.send(WsMessage::Text("Expected auth as text, got binary".into())).await;
                return;
            }
            Some(Ok(WsMessage::Ping(p))) => {
                let _ = write.send(WsMessage::Pong(p)).await;
                let _ = write.send(WsMessage::Text("No auth message received".into())).await;
                return;
            }
            Some(Ok(WsMessage::Pong(_))) => {
                let _ = write.send(WsMessage::Text("No auth message received".into())).await;
                return;
            }
            Some(Ok(WsMessage::Close(_))) | None => {
                return;
            }
            Some(Ok(WsMessage::Frame(_))) => {
                let _ = write.send(WsMessage::Text("Unexpected Frame message".into())).await;
                return;
            }
            Some(Err(e)) => {
                eprintln!("WebSocket error during auth: {}", e);
                return;
            }
        };

        let auth_val: serde_json::Value = match serde_json::from_str(&initial_msg) {
            Ok(v) => v,
            Err(_) => {
                let _ = write.send(WsMessage::Text("Invalid JSON format for auth".into())).await;
                return;
            }
        };

        // We'll store the authenticated username here, once known.
        let mut authenticated_username: Option<String> = None;

        // Process authentication
        if let Err(e) = self.process_auth(&auth_val, &mut write, &mut authenticated_username).await {
            let _ = write.send(WsMessage::Text(e)).await;
            return;
        }

        // If no authenticated user by now, something is wrong.
        let username = match authenticated_username {
            Some(u) => u,
            None => {
                let _ = write.send(WsMessage::Text("No authenticated user".into())).await;
                return;
            }
        };

        // ---- STEP 2: SEND MESSAGE HISTORY ----
        if let Err(e) = self.send_message_history(&mut write).await {
            eprintln!("Failed to send message history: {}", e);
            self.disconnect_user(&username).await;
            return;
        }

        // ---- STEP 3: MAIN LOOP ----
        loop {
            tokio::select! {
                maybe_msg = read.next() => {
                    match maybe_msg {
                        Some(Ok(WsMessage::Text(msg))) => {
                            match serde_json::from_str::<Message>(&msg) {
                                Ok(message) => {
                                    self.broadcast_message(message).await;
                                }
                                Err(_) => {
                                    let _ = write.send(WsMessage::Text("Invalid message format".into())).await;
                                }
                            }
                        }
                        Some(Ok(WsMessage::Binary(_))) => {
                            let _ = write.send(WsMessage::Text("Binary messages not supported".into())).await;
                        }
                        Some(Ok(WsMessage::Ping(p))) => {
                            if write.send(WsMessage::Pong(p)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(WsMessage::Pong(_))) => {
                            // Ignore pongs
                        }
                        Some(Ok(WsMessage::Close(_))) => {
                            // Client requested to close
                            break;
                        }
                        Some(Ok(WsMessage::Frame(_))) => {
                            // Frame messages are low-level, ignore them
                            eprintln!("Received unsupported Frame message. Ignoring.");
                        }
                        Some(Err(e)) => {
                            eprintln!("WebSocket error: {}", e);
                            break;
                        }
                        None => {
                            // Client disconnected
                            break;
                        }
                    }
                }
                broadcast_msg = rx.recv() => {
                    match broadcast_msg {
                        Ok(msg) => {
                            if let Ok(json) = serde_json::to_string(&msg) {
                                if write.send(WsMessage::Text(json)).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(_) => {
                            // Broadcast channel closed unexpectedly
                            break;
                        }
                    }
                }
            }
        }

        println!("Client disconnected: {}", username);
        // Mark user as disconnected
        self.disconnect_user(&username).await;
    }

    async fn process_auth<W>(
        &self,
        auth: &serde_json::Value,
        write: &mut W,
        authenticated_username: &mut Option<String>,
    ) -> Result<(), String>
    where
        W: SinkExt<WsMessage, Error = tungstenite::Error> + Unpin,
    {
        let auth_type = auth["type"].as_str().ok_or("Missing auth type")?;
        let username = auth["username"].as_str().unwrap_or("").to_string();
        let password = auth["password"].as_str().unwrap_or("").to_string();

        match auth_type {
            "register" => {
                self.register_user(&username, &password)
                    .await
                    .map_err(|e| format!("Registration failed: {}", e))?;
                write
                    .send(WsMessage::Text("Registration successful".into()))
                    .await
                    .map_err(|_| "Failed to send registration success message")?;
                // Registration does not log the user in. They must send a login message after this.
            }
            "login" => {
                if self.authenticate_user(&username, &password).await {
                    // Check if this user is already connected
                    {
                        let mut connected = self.connected_users.lock().await;
                        if connected.contains(&username) {
                            return Err("User is already connected".into());
                        }
                        // Mark user as connected
                        connected.insert(username.clone());
                    }

                    write
                        .send(WsMessage::Text("Authentication successful".into()))
                        .await
                        .map_err(|_| "Failed to send authentication success message")?;

                    *authenticated_username = Some(username);
                } else {
                    return Err("Authentication failed".into());
                }
            }
            _ => return Err("Invalid authentication type".into()),
        }

        Ok(())
    }

    async fn send_message_history<W>(&self, write: &mut W) -> Result<(), tungstenite::Error>
    where
        W: SinkExt<WsMessage, Error = tungstenite::Error> + Unpin,
    {
        let messages = self.messages.lock().await;
        for msg in messages.iter() {
            let json = match serde_json::to_string(msg) {
                Ok(j) => j,
                Err(_) => continue,
            };
            write.send(WsMessage::Text(json)).await?;
        }
        Ok(())
    }

    // Removes the given user from the connected_users set
    async fn disconnect_user(&self, username: &str) {
        let mut connected = self.connected_users.lock().await;
        connected.remove(username);
    }
}

#[tokio::main]
async fn main() {
    let server = Arc::new(ChatServer::new());
    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("Failed to bind to address");

    println!("WebSocket server listening on ws://0.0.0.0:8080");

    while let Ok((stream, _)) = listener.accept().await {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            server.handle_connection(stream).await;
        });
    }
}