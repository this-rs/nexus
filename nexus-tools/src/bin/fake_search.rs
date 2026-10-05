//! `fake_search [ADDR] [KEY]`: a local search server for tests (see `fake_search` in the library).

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:0".to_owned());
    let key = args.next().unwrap_or_else(|| "test-key".to_owned());
    if let Err(error) = nexus_tools::fake_search::serve_forever(&addr, &key).await {
        eprintln!("fake_search: {error}");
        std::process::exit(1);
    }
}
