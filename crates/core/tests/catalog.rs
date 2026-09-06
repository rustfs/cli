use rc_core::catalog::{CatalogTarget, ResourceKind};

#[test]
fn nested_namespace_is_not_an_object_key() {
    let target =
        CatalogTarget::parse("local/analytics/sales.eu/orders", ResourceKind::Table).unwrap();
    assert_eq!(target.namespace, vec!["sales", "eu"]);
    assert_eq!(target.name.as_deref(), Some("orders"));
}

#[test]
fn malformed_resources_are_rejected() {
    for path in [
        "local/b/n/t/extra",
        "local/b/n%2Fother/t",
        "local/b/../t",
        "local/b/n/",
        "local/b/n/T",
    ] {
        assert!(
            CatalogTarget::parse(path, ResourceKind::Table).is_err(),
            "{path}"
        );
    }
}

#[test]
fn warehouse_preserves_bucket_dots_without_accepting_path_syntax() {
    let target = CatalogTarget::parse("local/data.warehouse/ns/t", ResourceKind::Table).unwrap();
    assert_eq!(target.warehouse, "data.warehouse");
    for bucket in ["..", ".", "a?b", "a%2Fb", "a#b"] {
        assert!(CatalogTarget::parse(&format!("local/{bucket}"), ResourceKind::Warehouse).is_err());
    }
}
