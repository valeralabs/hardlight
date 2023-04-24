use std::collections::HashMap;

use hardlight::*;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = ServerConfig::new_self_signed("localhost:8080");
    let mut server = TodoServer::new(config);
    server.start().await.unwrap();

    let mut client = TodoClient::new_self_signed("localhost:8080");
    client.connect().await.unwrap();

    let tasks_to_create = vec![
        Task::new("Buy milk"),
        Task::new("Buy eggs"),
        Task::new("Buy bread"),
        Task::new("use arch btw"),
    ];

    print_tasks(&client).await;

    let len = tasks_to_create.len();
    let start = std::time::Instant::now();
    for task in tasks_to_create {
        client.create(task).await.unwrap();
    }
    println!("Created {} tasks in {:?}", len, start.elapsed());

    let task_id = client.create(Task::new("Buy cheese")).await.unwrap();

    print_tasks(&client).await;

    client.delete(vec![task_id]).await.unwrap();

    print_tasks(&client).await;

    client.mark_as_done(vec![0, 1]).await.unwrap();

    print_tasks(&client).await;

    client.delete_all().await.unwrap();

    print_tasks(&client).await;

    client.disconnect();
    server.stop();
}

async fn print_tasks(client: &TodoClient) {
    let tasks = client.get_all().await.unwrap();
    if tasks.is_empty() {
        println!("No tasks");
    } else {
        for task in tasks {
            println!("{:?}", task);
        }
    }

    println!();
}

/// These RPC methods are executed on the server and can be called by clients.
#[rpc]
trait Todo {
    async fn create(&self, task: Task) -> HandlerResult<u32>;
    async fn mark_as_done(&self, id: Vec<u32>) -> HandlerResult<()>;
    async fn get(&self, ids: Vec<u32>) -> HandlerResult<Vec<Task>>;
    async fn get_all(&self) -> HandlerResult<Vec<Task>>;
    async fn delete(&self, ids: Vec<u32>) -> HandlerResult<()>;
    async fn delete_all(&self) -> HandlerResult<()>;
}

#[codable]
enum TaskStatus {
    Todo,
    Done,
}

#[codable]
struct Task {
    text: String,
    status: TaskStatus,
}

impl Task {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            status: TaskStatus::Todo,
        }
    }
}

#[connection_state]
struct State {
    tasks: HashMap<u32, Task>,
    next_id: u32,
}

#[rpc_handler]
impl Todo for Handler {
    async fn create(&self, task: Task) -> HandlerResult<u32> {
        let mut state = self.state.lock();
        let id = state.next_id;
        state.next_id += 1;
        state.tasks.insert(id, task);
        println!("State: {:?}", *state);
        Ok(id)
    }
    async fn mark_as_done(&self, ids: Vec<u32>) -> HandlerResult<()> {
        let mut state = self.state.lock();
        for id in ids {
            if let Some(task) = state.tasks.get_mut(&id) {
                task.status = TaskStatus::Done;
            }
        }
        Ok(())
    }
    async fn get(&self, ids: Vec<u32>) -> HandlerResult<Vec<Task>> {
        let state = self.state.lock();
        let mut tasks = Vec::new();
        for id in ids {
            if let Some(task) = state.tasks.get(&id) {
                tasks.push(task.clone());
            }
        }
        Ok(tasks)
    }
    async fn get_all(&self) -> HandlerResult<Vec<Task>> {
        let state = self.state.lock();
        let mut tasks = Vec::new();
        for task in state.tasks.values() {
            tasks.push(task.clone());
        }
        Ok(tasks)
    }
    async fn delete(&self, ids: Vec<u32>) -> HandlerResult<()> {
        let mut state = self.state.lock();
        for id in ids {
            state.tasks.remove(&id);
        }
        Ok(())
    }
    async fn delete_all(&self) -> HandlerResult<()> {
        let mut state = self.state.lock();
        state.tasks.clear();
        Ok(())
    }
}
