//
// Main entry point for the Page Server executable
//

use std::thread;
use log::*;

use pageserver::page_service;
use pageserver::restore_s3;
use pageserver::walreceiver;
use pageserver::walredo;

use std::io::Error;

fn main() -> Result<(), Error> {
    let mut threads = Vec::new();

    // Initialize logger
    stderrlog::new()
        .verbosity(3)
        .module("pageserver")
        .init().unwrap();
    info!("starting...");

    // Initialize the WAL applicator
    let walredo_thread = thread::Builder::new()
        .name("WAL redo thread".into()).spawn(
            || {
                walredo::wal_applicator_main();
            }).unwrap();
    threads.push(walredo_thread);
    
    // Before opening up for connections, restore the latest base backup from S3.
    // (We don't persist anything to local disk at the moment, so we need to do
    // this at every startup)
    restore_s3::restore_main();

    // Launch the WAL receiver thread. It will try to connect to the WAL safekeeper,
    // and stream the WAL. If the connection is lost, it will reconnect on its own.
    // We just fire and forget it here.
    let walreceiver_thread = thread::Builder::new()
        .name("WAL receiver thread".into()).spawn(
            || {
                // thread code
                walreceiver::thread_main();
            }).unwrap();
    threads.push(walreceiver_thread);

    // GetPage@LSN requests are served by another thread. (It uses async I/O,
    // but the code in page_service sets up it own thread pool for that)
    let page_server_thread = thread::Builder::new()
        .name("Page Service thread".into()).spawn(
            || {
                // thread code
                page_service::thread_main();
            }).unwrap();
    threads.push(page_server_thread);

    // never returns.
    for t in threads {
        t.join().unwrap()
    }
    Ok(())
}