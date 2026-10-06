fn main() {
    // Flat, renderer-agnostic widgets; our own theme tokens style the rest.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".to_string());
    if let Err(err) = slint_build::compile_with_config("ui/app.slint", config) {
        panic!("failed to compile ui/app.slint: {err}");
    }
}
