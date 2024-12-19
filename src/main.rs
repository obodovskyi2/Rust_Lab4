use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async};
use futures::{StreamExt, SinkExt};
use serde::{Serialize, Deserialize};
use tokio::sync::{broadcast, Mutex};

#[derive(Clone, Serialize, Deserialize)]
struct Message {
    from: String,
    content: String,
    timestamp: i64,
}

struct ChatServer {
    messages: Arc<Mutex<Vec<Message>>>,
    users: Arc<Mutex<HashMap<String, String>>>, // username -> password
    broadcaster: broadcast::Sender<Message>,
}

impl ChatServer {
    fn new() -> Self {
        let (broadcaster, _) = broadcast::channel(100);
        ChatServer {
            messages: Arc::new(Mutex::new(Vec::new())),
            users: Arc::new(Mutex::new(HashMap::new())),
            broadcaster,
        }
    }

    async fn register_user(&self, username: &str, password: &str) -> Result<(), String> {
        let mut users = self.users.lock().await;
        if users.contains_key(username) {
            Err("Username already exists".to_string())
        } else {
            users.insert(username.to_string(), password.to_string());
            Ok(())
        }
    }

    async fn authenticate_user(&self, username: &str, password: &str) -> bool {
        let users = self.users.lock().await;
        users.get(username).map_or(false, |stored_password| stored_password == password)
    }

    async fn broadcast_message(&self, message: Message) {
        let mut messages = self.messages.lock().await;
        messages.push(message.clone());
        if let Err(e) = self.broadcaster.send(message) {
            eprintln!("Failed to broadcast message: {}", e);
        }
    }

    async fn handle_connection(&self, stream: TcpStream) {
        match accept_async(stream).await {
            Ok(ws_stream) => {
                let (mut writer, mut reader) = ws_stream.split();
                let mut receiver = self.broadcaster.subscribe();

                if let Some(Ok(raw_msg)) = reader.next().await {
                    if raw_msg.is_text() {
                        if let Ok(auth_request) = serde_json::from_str::<HashMap<String, String>>(raw_msg.to_text().unwrap()) {
                            match auth_request.get("type").map(String::as_str) {
                                Some("register") => {
                                    let username = auth_request["username"].clone();
                                    let password = auth_request["password"].clone();
                                    let response = match self.register_user(&username, &password).await {
                                        Ok(_) => "Registration successful".to_string(),
                                        Err(e) => e,
                                    };
                                    let _ = writer.send(response.into()).await;
                                }
                                Some("login") => {
                                    let username = auth_request["username"].clone();
                                    let password = auth_request["password"].clone();
                                    let response = if self.authenticate_user(&username, &password).await {
                                        "Authentication successful"
                                    } else {
                                        "Authentication failed"
                                    };
                                    let _ = writer.send(response.into()).await;
                                }
                                _ => {
                                    let _ = writer.send("Invalid request type".into()).await;
                                    return;
                                }
                            }
                        } else {
                            let _ = writer.send("Invalid authentication request".into()).await;
                            return;
                        }
                    } else {
                        eprintln!("Received a non-text message, ignoring.");
                    }
                }

                let history = self.messages.lock().await.clone();
                for msg in history {
                    if let Ok(serialized) = serde_json::to_string(&msg) {
                        let _ = writer.send(serialized.into()).await;
                    }
                }

                loop {
                    tokio::select! {
                        Some(Ok(raw_msg)) = reader.next() => {
                            if raw_msg.is_text() {
                                if let Ok(message) = serde_json::from_str::<Message>(raw_msg.to_text().unwrap()) {
                                    self.broadcast_message(message).await;
                                } else {
                                    eprintln!("Failed to parse message");
                                }
                            }
                        }
                        Ok(message) = receiver.recv() => {
                            if let Ok(serialized) = serde_json::to_string(&message) {
                                let _ = writer.send(serialized.into()).await;
                            }
                        }
                    }
                }
            }
            Err(e) => eprintln!("Failed to accept websocket connection: {}", e),
        }
    }
}

#[tokio::main]
async fn main() {
    let server = Arc::new(ChatServer::new());
    let listener = TcpListener::bind("127.0.0.1:8080").await.expect("Failed to bind server");

    println!("WebSocket server listening on ws://127.0.0.1:8080");

    while let Ok((stream, _)) = listener.accept().await {
        let server_clone = Arc::clone(&server);
        tokio::spawn(async move {
            server_clone.handle_connection(stream).await;
        });
    }
}
