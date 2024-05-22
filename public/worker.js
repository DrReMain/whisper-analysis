import init, { Decoder } from "./wasm/whisper.js";

const model = [
  "https://huggingface.co/openai/whisper-tiny/resolve/main/model.safetensors",
  // "https://huggingface.co/openai/whisper-tiny/resolve/main/tokenizer.json",
  // "https://huggingface.co/openai/whisper-tiny/resolve/main/config.json"
  // "model/model.safetensors",
  "model/tokenizer.json",
  "model/config.json",
  "model/mel_filters.safetensors",
];

self.addEventListener("message", async function (event) {
  const [weights, tokenizer, config, mel_filters] = (
    await Promise.all(
      (
        await Promise.all(model.map((url) => fetch(url)))
      ).map((_) => _.arrayBuffer())
    )
  ).map((__) => new Uint8Array(__));
  await init();
  // tiny_multilingual
  const instance = new Decoder(
    weights,
    tokenizer,
    mel_filters,
    config,
    false,
    true,
    true,
    null,
    null
  );

  const audio = new Uint8Array(
    await (await fetch(event.data.audioSrc)).arrayBuffer()
  );
  const data = instance.decode(audio);
  self.postMessage({
    status: "complete",
    output: JSON.parse(data),
  });
});
