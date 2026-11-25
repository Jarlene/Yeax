let meta = #{
    name: "hello-bundled",
    description: "Bundled sample installed into $CODEX_HOME/bundled/workflows on first use",
    when_to_use: "Verify the bundled scope and its create-only install state machine.",
    phases: [
        #{ title: "Greet", detail: "Echo the installed bundled script" },
    ],
};

phase("Greet");
complete(#{ ok: true, scope: "bundled", name: meta.name });