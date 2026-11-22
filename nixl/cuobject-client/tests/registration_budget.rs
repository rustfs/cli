#![cfg(feature = "client")]

use rustfs_cuobject_client::Client;
use std::sync::Arc;

#[test]
#[ignore = "requires an RDMA device and the failed_deregistration.cc test library in LD_PRELOAD"]
fn failed_descriptor_release_keeps_the_accounting_owner() {
    let client = Client::open().unwrap();
    let budget = Arc::new(());
    let retained = Arc::downgrade(&budget);
    let window = client.register(1 << 20, budget).unwrap();
    drop(window);
    assert_eq!(retained.strong_count(), 1);
}
