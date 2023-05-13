use hardlight::*;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};
use tracing::info;

use crate::service::{Counter, CounterClient, CounterServer};

mod handler;
mod service;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt::init();

    let config = ServerConfig::new_self_signed("localhost:8080");
    info!("{:?}", config);
    let mut server = CounterServer::new(config);
    server.start().await.unwrap();

    // wait for the server to start
    sleep(Duration::from_millis(10)).await;

    let mut client = CounterClient::new_self_signed("localhost:8080");
    // client.add_event_handler(EventMonitor::init()).await;
    client.connect().await.unwrap();

    let _ = client.increment(1).await;

    client.disconnect(); // demonstrate that we can disconnect and reconnect
    server.stop();
    server.start().await.unwrap();
    client.connect().await.unwrap(); // note: state is reset as we're using a new connection

    assert!(client.get().await.unwrap() == 0);

    let num_tasks = 1;
    let num_increments_per_task = 1;
    info!("Incrementing counter using {num_tasks} tasks with {num_increments_per_task} increments each");
    let first_value = client.get().await.expect("get failed");
    info!("First value: {}", first_value);

    let counter = Arc::new(client);

    let start = Instant::now();

    let mut tasks = Vec::new();
    for _ in 0..num_tasks {
        let counter = counter.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..num_increments_per_task {
                let _ = counter.increment(1).await;
            }
        }));
    }

    for task in tasks {
        task.await.expect("task failed");
    }

    let final_value = counter.get().await.expect("get failed");

    let elapsed = start.elapsed();
    info!(
        "Ran {} increments using {} tasks in {:?}",
        num_tasks * num_increments_per_task,
        num_tasks,
        elapsed
    );
    info!(
        "(mean time/task = {:?})",
        elapsed / (num_tasks * num_increments_per_task)
    );

    info!("Final value: {}", final_value);

    // make sure server-side mutex is working...
    assert!(final_value == first_value + (num_tasks * num_increments_per_task));

    Ok(())
}
