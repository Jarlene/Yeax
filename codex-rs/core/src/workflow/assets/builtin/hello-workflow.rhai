let meta = #{
    name: "hello-workflow",
    description: "Minimal workflow that asks one child agent for a greeting and returns its reply",
    when_to_use: "Smoke-test the workflow runtime end to end with a single child agent.",
    phases: [
        #{ title: "Greet", detail: "Ask one child agent for a short greeting" },
    ],
};

let target = "world";
if args != () && args.name != () {
    target = args.name.to_string();
}

phase("Greet");
log("asking one agent to greet " + target);
let reply = agent("Reply with a single short greeting for " + target + ".", #{
    label: "greeter",
    phase: "Greet",
});

if reply.success {
    complete(#{ ok: true, greeting: reply.output });
} else {
    complete(#{ ok: false, agent_id: reply.agent_id });
}