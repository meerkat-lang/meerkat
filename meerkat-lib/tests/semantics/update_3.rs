use super::helpers::check_update;

#[tokio::test]
async fn test_code_updates_3() {
    let initial = r#"
        service s1 {
            var a = 0;
            var b = 0;
            def c = a + b;
        }
    "#;
    let update = r#"
        update s1 {
            def c = a(b); // apply b to a, should not pass
        }
    "#;
    let res = check_update(initial, update).await;
    assert!(
        res.is_err(),
        "Expected apply of non-function to fail, but got: {:?}",
        res
    );
}
