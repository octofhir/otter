use otter_vm_runtime::Otter;

#[test]
fn async_generator_resumes_after_await_fulfillment() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let log = [];
        async function* gen() {
            const a = await Promise.resolve(7);
            log.push("after-await-1");
            yield a + 1;
            const b = await Promise.resolve(10);
            log.push("after-await-2");
            return b + 2;
        }

        const it = gen();
        it.next()
          .then(r => {
              log.push(`first:${r.value}:${r.done}`);
              return it.next();
          })
          .then(r => {
              log.push(`second:${r.value}:${r.done}`);
          });

        Promise.resolve().then(() => {}).then(() => {
            const got = log.join("|");
            const expected = "after-await-1|first:8:false|after-await-2|second:12:true";
            if (got !== expected) throw new Error(`unexpected log: ${got}`);
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed: {:?}", result.err());
}

#[test]
fn async_generator_resumes_after_await_rejection() {
    let mut otter = Otter::new();
    let result = otter.eval_sync(
        r#"
        let log = [];
        async function* gen() {
            try {
                await Promise.reject("boom");
                yield "unreachable";
            } catch (e) {
                yield "caught:" + e;
            }
            return 2;
        }

        const it = gen();
        it.next()
          .then(r => {
              log.push(`first:${r.value}:${r.done}`);
              return it.next();
          })
          .then(r => {
              log.push(`second:${r.value}:${r.done}`);
          });

        Promise.resolve().then(() => {}).then(() => {
            const got = log.join("|");
            const expected = "first:caught:boom:false|second:2:true";
            if (got !== expected) throw new Error(`unexpected log: ${got}`);
        });
        "#,
    );

    assert!(result.is_ok(), "eval_sync should succeed: {:?}", result.err());
}
