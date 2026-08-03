use super::helpers::check_update;

#[tokio::test]
async fn test_code_updates_1() {
    let initial = r#"
        service s1 {
            var a = 0;
            var b = 0;
            def c = (fn (x: int) => x)(a);
        }
    "#;
    let update = r#"
        update s1 {
            var a = ""; // compatible redefine, should pass
        }
    "#;
    let res = check_update(initial, update).await;
    assert!(
        res.is_ok(),
        "Expected compatible redefine to pass, but got: {:?}",
        res
    );
}
