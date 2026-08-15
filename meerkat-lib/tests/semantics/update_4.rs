use super::helpers::check_update;

#[tokio::test]
async fn test_code_updates_4() {
    let initial = r#"
        service s1 {
            var a = 0;
            var b = 0;
            def c = a + b;
        }
    "#;
    let update = r#"
        update s1 {
            def a = c; // introduce cycle, should not pass
        }
    "#;
    let res = check_update(initial, update).await;
    assert!(
        res.is_err(),
        "Expected dependency cycle to fail, but got: {:?}",
        res
    );
}
