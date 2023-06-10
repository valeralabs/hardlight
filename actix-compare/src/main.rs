use std::{fs::File, io::BufReader, time::Instant};

use actix_web::{get, App, HttpResponse, HttpServer, Responder, middleware::Compress};
use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls_pemfile::{certs, pkcs8_private_keys};

#[get("/")]
async fn hello() -> impl Responder {
    HttpResponse::Ok().json("")
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // spawn a client connection that will make 10,000 calls to the server
    tokio::spawn(async move {
        // wait for the server to start
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = reqwest::Client::new();
        // pre-connect to the server
        let _ = client.head("https://localhost:8443").send().await;
        for _ in 0..100_000 {
            let _ = client.get("https://localhost:8443").send().await;
        }
        for _ in 0..10_000 {
            let start = Instant::now();
            let _ = client.get("https://localhost:8443").send().await;
            let elapsed = start.elapsed();
            println!("{:?}", elapsed.as_micros());
        }

        // run 10k requests in parallel using 10 threads
        // let mut handles = Vec::new();
        // for _ in 0..100 {
        //     let client = client.clone();
        //     handles.push(tokio::spawn(async move {
        //         for _ in 0..1_00 {
        //             let start = Instant::now();
        //             let _ = client.get("https://localhost:8443").send().await;
        //             let elapsed = start.elapsed();
        //             println!("{:?}", elapsed.as_micros());
        //         }
        //     }));
        // }
        // for handle in handles {
        //     handle.await.unwrap();
        // }

        std::process::exit(0);
    });

    let config = load_rustls_config();

    HttpServer::new(|| App::new().service(hello).wrap(Compress::default()))
        .bind_rustls("127.0.0.1:8443", config)?
        .run()
        .await
}
fn load_rustls_config() -> rustls::ServerConfig {
    // init server config builder with safe defaults
    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth();

    // load TLS key/cert files
    let cert_file = &mut BufReader::new(File::open("cert.pem").unwrap());
    let key_file = &mut BufReader::new(File::open("key.pem").unwrap());

    // convert files to key/cert objects
    let cert_chain = certs(cert_file)
        .unwrap()
        .into_iter()
        .map(Certificate)
        .collect();
    let mut keys: Vec<PrivateKey> = pkcs8_private_keys(key_file)
        .unwrap()
        .into_iter()
        .map(PrivateKey)
        .collect();

    // exit if no keys could be parsed
    if keys.is_empty() {
        eprintln!("Could not locate PKCS 8 private keys.");
        std::process::exit(1);
    }

    config.with_single_cert(cert_chain, keys.remove(0)).unwrap()
}
