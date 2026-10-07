# LLM proxy response fixtures

`openai-shape/` holds a chat-completion and an embeddings response in the
OpenAI-compatible shape a typical LLM proxy documents (proxy v0.3.275,
2026-09-28). They were written from that documentation, not captured: no
agent session has a proxy key. After the first live check, capture one real
reply of each into a folder named after the proxy version (for example
`0.3.275/chat.json`, with the key and any user name removed) and add it to
`core/llm-proxy.test.js`'s fixture list — both folders must keep passing.
