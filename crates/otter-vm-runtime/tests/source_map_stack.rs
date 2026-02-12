use otter_vm_runtime::Otter;

#[test]
fn error_stack_frames_include_line_and_column_from_source_map() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let capturedLine = 0;
        let capturedColumn = 0;

        function boom() {
            throw new Error("boom");
        }

        try {
            boom();
        } catch (e) {
            const frames = e.__stack_frames__;
            if (!frames || frames.length === 0) {
                throw new Error("missing stack frames");
            }
            capturedLine = frames[0].line;
            capturedColumn = frames[0].column;
        }

        if (!(capturedLine >= 2)) {
            throw new Error("line was not mapped from source");
        }
        if (!(capturedColumn >= 1)) {
            throw new Error("column was not mapped from source");
        }
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed: {:?}", result.err());
}
