import init, * as agent from "./pkg/agent_web.js";

await init();

export const greet = agent.greet;
