use super::helpers::check_program;

#[test]
fn test_dependency_analysis_1() {
    let code = r#"
        service s1 {
            var b = 0;
            def app = fn (f: (int) -> int) => fn (x: int) => f(x + b);
            def d1 = app(fn (x: int) => x + a); 
            def d2 = d1(0); 
            var a = 0; // illegal forward reference!
        }
    "#;
    let res = check_program(code);
    assert!(
        res.is_err(),
        "Expected illegal forward reference to fail: {:?}",
        res
    );
}
