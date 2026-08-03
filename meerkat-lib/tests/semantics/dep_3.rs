use super::helpers::check_program;

#[test]
fn test_dependency_analysis_3() {
    let code = r#"
        service s1 {
            def add_a = fn (x: int) => x + a;
            def my_add = fn (x: int) => add_a(x);
            var a = 0; // should be safe!
        }
    "#;
    let res = check_program(code);
    assert!(
        res.is_ok(),
        "Expected compilation to succeed, but got: {:?}",
        res
    );
}
