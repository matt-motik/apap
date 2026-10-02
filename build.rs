fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    // Ошибка компиляции UI проваливает сборку через код возврата, без паники (ТЗ-101).
    slint_build::compile_with_config("ui/app.slint", config)?;
    Ok(())
}
