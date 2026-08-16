use otter_node::{NodeApiBuilderExt, hosted_modules};
use otter_runtime::{CapabilitySet, Permission, Runtime};

#[test]
fn hosted_node_module_specs_are_static_and_ordered() {
    let specs = hosted_modules();
    // `fs` leads the list; the exact length grows as modules are ported.
    assert_eq!(specs[0].specifier(), "node:fs");
    assert_eq!(specs[1].specifier(), "fs");

    // Every `node:`-prefixed builtin has a matching bare specifier.
    let names: Vec<&str> = specs.iter().map(|m| m.specifier()).collect();
    for name in &names {
        if let Some(bare) = name.strip_prefix("node:") {
            assert!(names.contains(&bare), "missing bare specifier for {name}");
        }
    }
    // Core modules ported so far are registered.
    for expected in ["os", "node:os", "node:test", "assert", "path"] {
        assert!(
            names.contains(&expected),
            "missing hosted module {expected}"
        );
    }
}

#[test]
fn node_fs_requires_read_permission() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "secret").unwrap();
    std::fs::write(
        &main,
        r#"
            import { readFileSync } from "node:fs";
            readFileSync("data.txt", "utf8");
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder().with_node_apis().build().unwrap();
    let err = runtime.run_module(&main).unwrap_err();
    assert!(err.to_string().contains("permission denied"));
}

#[test]
fn node_fs_read_write_round_trips_with_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(
        &main,
        format!(
            r#"
            import {{ existsSync, readFileSync, writeFileSync }} from "node:fs";
            writeFileSync({path:?}, "hello", "utf8");
            if (!existsSync({path:?})) {{
                throw new Error("exists failed");
            }}
            if (readFileSync({path:?}, "utf8") !== "hello") {{
                throw new Error("read failed");
            }}
        "#,
            path = data.to_string_lossy()
        ),
    )
    .unwrap();
    let caps = CapabilitySet {
        read: Permission::allow([data.clone()]),
        write: Permission::allow([data]),
        ..CapabilitySet::sandbox()
    };

    let mut runtime = Runtime::builder()
        .capabilities(caps)
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// The filesystem members added for Node's own tests: a private temporary
/// directory, both link kinds, ownership-free timestamp changes, and the
/// descriptor operations that used to be silent no-ops.
#[test]
fn node_fs_links_timestamps_and_descriptor_operations() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        format!(
            r#"
            import {{
                closeSync, existsSync, fsyncSync, ftruncateSync, linkSync, mkdtempSync,
                openSync, readFileSync, readlinkSync, statSync, symlinkSync, utimesSync,
                writeFileSync,
            }} from "node:fs";
            import {{ join }} from "node:path";

            const root = {root:?};
            const scratch = mkdtempSync(join(root, "otter-"));
            if (!existsSync(scratch)) throw new Error("mkdtemp did not create a directory");
            if (!scratch.startsWith(join(root, "otter-"))) {{
                throw new Error("mkdtemp ignored its prefix: " + scratch);
            }}
            if (mkdtempSync(join(root, "otter-")) === scratch) {{
                throw new Error("mkdtemp handed out the same name twice");
            }}

            const file = join(scratch, "a.txt");
            writeFileSync(file, "hello");
            const hard = join(scratch, "hard.txt");
            linkSync(file, hard);
            if (readFileSync(hard, "utf8") !== "hello") throw new Error("hard link lost content");

            const soft = join(scratch, "soft.txt");
            symlinkSync(file, soft);
            if (readlinkSync(soft) !== file) throw new Error("symlink target: " + readlinkSync(soft));

            utimesSync(file, 1600000000, 1600000001);
            const stamped = statSync(file);
            if (Math.round(stamped.mtimeMs / 1000) !== 1600000001) {{
                throw new Error("mtime was " + stamped.mtimeMs);
            }}

            const fd = openSync(file, "r+");
            ftruncateSync(fd, 1);
            fsyncSync(fd);
            closeSync(fd);
            if (readFileSync(file, "utf8") !== "h") {{
                throw new Error("ftruncate left " + readFileSync(file, "utf8"));
            }}
        "#,
            root = dir.path().to_string_lossy()
        ),
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `fsPromises.open` hands out a handle whose reads, writes, metadata, and
/// web stream all address the same descriptor, and whose methods refuse to run
/// once it is closed.
#[test]
fn node_fs_promises_file_handle_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "hello world").unwrap();
    std::fs::write(
        &main,
        format!(
            r#"
            import {{ open }} from "node:fs/promises";

            const handle = await open({path:?}, "r");
            if (typeof handle.fd !== "number") throw new Error("fd was " + typeof handle.fd);

            const {{ bytesRead, buffer }} = await handle.read(Buffer.alloc(5), 0, 5, 0);
            if (bytesRead !== 5) throw new Error("bytesRead was " + bytesRead);
            if (buffer.toString("utf8", 0, bytesRead) !== "hello") {{
                throw new Error("read " + buffer.toString("utf8", 0, bytesRead));
            }}

            const stat = await handle.stat();
            if (stat.size !== 11) throw new Error("size was " + stat.size);

            await handle.close();
            let afterClose;
            try {{
                await handle.read(Buffer.alloc(1), 0, 1, 0);
            }} catch (error) {{
                afterClose = error;
            }}
            if (afterClose?.code !== "EBADF") {{
                throw new Error("a closed handle answered " + afterClose?.code);
            }}
            await handle.close();
        "#,
            path = data.to_string_lossy()
        ),
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// Key derivation answers the RFC vectors byte for byte, and its argument
/// checks carry the codes Node's own tests assert.
#[test]
fn node_crypto_key_derivation_matches_the_published_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import { pbkdf2Sync, hkdfSync } from "node:crypto";

        // RFC 6070 PBKDF2-HMAC-SHA1.
        const first = pbkdf2Sync("password", "salt", 1, 20, "sha1").toString("hex");
        if (first !== "0c60c80f961f0e71f3a9b524af6012062fe037a6") {
            throw new Error("pbkdf2 sha1: " + first);
        }
        const long = pbkdf2Sync(
            "passwordPASSWORDpassword",
            "saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, 25, "sha1").toString("hex");
        if (long !== "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038") {
            throw new Error("pbkdf2 long: " + long);
        }
        const sha256 = pbkdf2Sync("password", "salt", 4096, 32, "sha256").toString("hex");
        if (sha256 !== "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a") {
            throw new Error("pbkdf2 sha256: " + sha256);
        }

        const derived = Buffer.from(hkdfSync("sha256", "secret", "salt", "info", 42))
            .toString("hex");
        if (derived.length !== 84) throw new Error("hkdf length: " + derived.length);

        const checks = [
            [() => pbkdf2Sync("p", "s", 0, 20, "sha1"), "ERR_OUT_OF_RANGE"],
            [() => pbkdf2Sync("p", "s", 1, "20", "sha1"), "ERR_INVALID_ARG_TYPE"],
            [() => pbkdf2Sync("p", "s", 1, 20), "ERR_INVALID_ARG_TYPE"],
            [() => pbkdf2Sync("p", "s", 1, 20, "md55"), "ERR_CRYPTO_INVALID_DIGEST"],
            [() => pbkdf2Sync(1, "s", 1, 20, "sha1"), "ERR_INVALID_ARG_TYPE"],
        ];
        for (const [call, code] of checks) {
            let thrown;
            try { call(); } catch (error) { thrown = error; }
            if (thrown?.code !== code) throw new Error(code + " got " + thrown?.code);
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// A store entered with `AsyncLocalStorage.run` is still current inside the
/// continuations scheduled from that scope, and absent outside it. The timer
/// leg needs a host scheduler, so it is covered by the Node corpus rather than
/// here.
#[test]
fn node_async_local_storage_survives_every_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import { AsyncLocalStorage } from "node:async_hooks";

        const storage = new AsyncLocalStorage();
        const seen = [];

        await storage.run({ id: 7 }, async () => {
            seen.push(["sync", storage.getStore()?.id]);
            const microtask = Promise.resolve().then(() => {
                seen.push(["microtask", storage.getStore()?.id]);
            });
            const resumed = (async () => {
                await null;
                seen.push(["await", storage.getStore()?.id]);
            })();
            await Promise.all([microtask, resumed]);
        });

        if (storage.getStore() !== undefined) throw new Error("the store escaped its scope");
        for (const [where, id] of seen) {
            if (id !== 7) throw new Error(where + " saw " + id);
        }
        if (seen.length !== 3) throw new Error("recorded " + JSON.stringify(seen));

        // A second store must not see the first one's binding.
        const other = new AsyncLocalStorage();
        storage.run({ id: 1 }, () => {
            if (other.getStore() !== undefined) throw new Error("stores are not independent");
        });
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// A domain owns the errors of the work started inside it: a synchronous
/// throw, and one raised from a continuation the domain never sees directly.
#[test]
fn node_domain_claims_errors_from_its_own_work() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import domain from "node:domain";

        const caught = [];
        const sync = domain.create();
        sync.on("error", (error) => caught.push(["sync", error.message]));
        sync.run(() => { throw new Error("boom"); });

        if (caught.length !== 1) throw new Error("sync throw was not claimed");
        if (caught[0][1] !== "boom") throw new Error("claimed " + caught[0][1]);

        const nested = domain.create();
        nested.run(() => {
            if (process.domain !== nested) throw new Error("process.domain is not the active one");
        });
        if (process.domain === nested) throw new Error("the domain outlived its run");

        const bound = domain.create();
        let boundError;
        bound.on("error", (error) => { boundError = error; });
        bound.bind(() => { throw new Error("bound"); })();
        if (boundError?.message !== "bound") throw new Error("bind did not route");

        const intercepted = domain.create();
        let interceptedError;
        intercepted.on("error", (error) => { interceptedError = error; });
        intercepted.intercept(() => { throw new Error("never runs"); })(new Error("first-arg"));
        if (interceptedError?.message !== "first-arg") throw new Error("intercept did not route");
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `dns` rejects the arguments Node rejects and exposes the platform's own
/// hint flags. Resolution itself needs a host timer to deliver its callback,
/// so it is covered by the Node corpus rather than here.
#[test]
fn node_dns_lookup_resolves_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import dns from "node:dns";

        let wrongType;
        try { dns.lookup(1, {}); } catch (error) { wrongType = error; }
        if (wrongType?.code !== "ERR_INVALID_ARG_TYPE") throw new Error("code " + wrongType?.code);
        if (wrongType.name !== "TypeError") throw new Error("name " + wrongType.name);

        let badOrder;
        try { dns.setDefaultResultOrder("sideways"); } catch (error) { badOrder = error; }
        if (badOrder?.code !== "ERR_INVALID_ARG_VALUE") throw new Error("code " + badOrder?.code);

        dns.setServers(["1.1.1.1"]);
        if (dns.getServers().join() !== "1.1.1.1") throw new Error("servers did not round-trip");

        // The hint flags are the platform's own, not invented ones.
        for (const flag of ["ADDRCONFIG", "V4MAPPED", "ALL"]) {
            if (typeof dns[flag] !== "number" || dns[flag] <= 0) {
                throw new Error(flag + " is " + dns[flag]);
            }
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `dgram`'s synchronous surface: the socket type it accepts, and the errors it
/// raises before it touches a socket. Binding, sending, and receiving all park
/// their work on a timer, which a bare runtime has no scheduler for, so those
/// legs are covered by the Node corpus rather than here.
#[test]
fn node_dgram_validates_before_it_opens_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import dgram from "node:dgram";

        function codeOf(run) {
            try { run(); } catch (error) { return error.code; }
            return undefined;
        }

        if (codeOf(() => dgram.createSocket("udp5")) !== "ERR_SOCKET_BAD_TYPE") {
            throw new Error("bad type is not rejected");
        }
        if (codeOf(() => dgram.createSocket({})) !== "ERR_SOCKET_BAD_TYPE") {
            throw new Error("missing type is not rejected");
        }

        // A string and an options object name the same socket.
        for (const options of ["udp4", { type: "udp4" }]) {
            const socket = dgram.createSocket(options);
            if (socket.type !== "udp4") throw new Error("type " + socket.type);
            if (!(socket instanceof dgram.Socket)) throw new Error("not a Socket");
        }

        const socket = dgram.createSocket("udp6");
        if (socket.type !== "udp6") throw new Error("type " + socket.type);

        // Nothing is open yet, so the state errors come before any syscall.
        if (codeOf(() => socket.address()) !== "ERR_SOCKET_DGRAM_NOT_RUNNING") {
            throw new Error("address() on an unbound socket");
        }
        if (codeOf(() => socket.remoteAddress()) !== "ERR_SOCKET_DGRAM_NOT_CONNECTED") {
            throw new Error("remoteAddress() on an unconnected socket");
        }
        if (codeOf(() => socket.disconnect()) !== "ERR_SOCKET_DGRAM_NOT_CONNECTED") {
            throw new Error("disconnect() on an unconnected socket");
        }
        if (codeOf(() => socket.send("payload")) !== "ERR_SOCKET_DGRAM_NOT_CONNECTED") {
            throw new Error("send() without a port on an unconnected socket");
        }

        // The port is checked before the connection state is.
        if (codeOf(() => socket.connect(0)) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("connect(0)");
        }
        if (codeOf(() => socket.connect(65536)) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("connect(65536)");
        }
        if (codeOf(() => socket.connectSync(1, "example.com")) !== "ERR_INVALID_ARG_VALUE") {
            throw new Error("connectSync with a name instead of an address");
        }
        if (codeOf(() => socket.bindSync({ address: "example.com" })) !== "ERR_INVALID_ARG_VALUE") {
            throw new Error("bindSync with a name instead of an address");
        }
        if (codeOf(() => socket.bindSync({ port: 65536 })) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("bindSync with an out-of-range port");
        }
        if (codeOf(() => socket.bindSync(null)) !== "ERR_INVALID_ARG_TYPE") {
            throw new Error("bindSync(null)");
        }

        // The option setters type-check their argument before reaching the socket.
        const wrongTypes = [
            () => socket.setTTL("2"),
            () => socket.setMulticastTTL("2"),
            () => socket.setBroadcast(1),
            () => socket.setMulticastInterface(1),
            () => socket.addMembership(1),
            () => socket.dropMembership(1),
        ];
        for (const run of wrongTypes) {
            if (codeOf(run) !== "ERR_INVALID_ARG_TYPE") {
                throw new Error("option setter accepted a wrong type: " + run);
            }
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `cluster` reports which role the process is in, and the primary's surface
/// is the one a program gets when nothing launched it as a worker. Forking a
/// worker starts a process and waits on a channel, neither of which a bare
/// runtime can do, so that half is covered by running the CLI.
#[test]
fn node_cluster_reports_the_primary_role() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import cluster from "node:cluster";

        if (cluster.isWorker) throw new Error("a process nobody forked is not a worker");
        if (!cluster.isPrimary) throw new Error("isPrimary " + cluster.isPrimary);
        if (cluster.isMaster !== cluster.isPrimary) throw new Error("isMaster disagrees");
        if (cluster.worker !== undefined) throw new Error("a primary is not a worker");

        // The primary keeps a registry, and a worker has none to keep.
        if (typeof cluster.workers !== "object" || cluster.workers === null) {
            throw new Error("workers registry is " + typeof cluster.workers);
        }
        if (Object.keys(cluster.workers).length !== 0) throw new Error("registry is not empty");

        for (const name of ["fork", "disconnect", "setupPrimary", "setupMaster"]) {
            if (typeof cluster[name] !== "function") throw new Error(name + " is missing");
        }
        if (typeof cluster.Worker !== "function") throw new Error("Worker class is missing");

        // Settings name the program a worker would run, which is this one.
        const settings = cluster.setupPrimary();
        if (settings.exec !== process.argv[1]) throw new Error("exec is " + settings.exec);
        if (cluster.settings !== settings) throw new Error("settings were not kept");

        // A primary schedules round-robin unless told otherwise, and the
        // policy names are the ones a program compares against.
        if (cluster.SCHED_RR === cluster.SCHED_NONE) throw new Error("policies collide");
        if (cluster.schedulingPolicy !== cluster.SCHED_RR) {
            throw new Error("policy " + cluster.schedulingPolicy);
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}
