import {type ChangeEvent, type DragEvent, useEffect, useRef, useState} from "react";

function App() {
    const ref = useRef<HTMLDivElement>(null);
    const worker = useRef<Worker>(new Worker("./worker.js", {type: "module"}));
    const [audioSrc, setAudioSrc] = useState('');
    const [text, setText] = useState('');
    useEffect(() => {
        worker.current.addEventListener('message', onMessage)
        return () => {
            worker.current.removeEventListener("message", onMessage)
        }
    }, [])

    const handleChange = ({target}: ChangeEvent<HTMLInputElement>) => {
        if (target.files?.length)
            setAudioSrc(URL.createObjectURL(target.files[0]))
    }
    const handleDrop = (e: DragEvent<HTMLDivElement>) => {
        e.preventDefault();
        ref.current?.classList.remove("border-blue-700");
        const url = e.dataTransfer.getData("text/uri-list");
        const files = e.dataTransfer.files;
        if (files.length) setAudioSrc(URL.createObjectURL(files[0]));
        else if (url) setAudioSrc(url);
    }
    const handleAnalysis = () => {
        if (!audioSrc) return;
        worker.current.postMessage({ audioSrc })
    }
    const onMessage = ({ data: {status, output} }: MessageEvent<{status: string; output: {dr:{text: string}}[]}>) => {
        if (status === "complete") {
            const text = output.map(_ => _.dr.text).join(" ");
            setText(text);
        }
    }

    return (
        <main className="bg-slate-900 w-screen h-screen flex flex-col gap-4 p-8">
            <h1 className="text-center text-3xl text-white">
                Whisper + candle(Rust) + React 基于 WebWorker + WASM
                <br/>
                纯前端实现音频转文字
            </h1>
            <section ref={ref} className="border-2 border-dashed rounded-2xl h-64 flex flex-col gap-2 p-8">
                <div
                    className="h-0 flex-1 flex justify-center items-center text-white"
                    onDragEnter={e => {e.preventDefault(); ref.current?.classList.add("border-blue-700")}}
                    onDragLeave={e => {e.preventDefault(); ref.current?.classList.remove("border-blue-700")}}
                    onDragOver={e => {e.preventDefault();ref.current?.classList.add("border-blue-700")}}
                    onDrop={handleDrop}
                >
                    <label htmlFor="file-upload">
                        拖动音频文件到这里或点击上传音频文件
                    </label>
                    <input id="file-upload" name="file-upload" type="file" accept="audio/*" className="sr-only"
                           onChange={handleChange}/>
                </div>
                <audio id="audio" controls className="w-full p-2 select-none" src={audioSrc} hidden={!audioSrc}/>
            </section>
            <button
                className="bg-white active:bg-white/50 disabled:bg-gray-500 disabled:active:bg-gray-500 rounded-2xl p-4 font-bold text-2xl"
                disabled={!audioSrc}
                onClick={handleAnalysis}
            >
                解析音频
            </button>
            <section className="h-0 flex-1 bg-slate-800 rounded-2xl p-8 text-white overflow-y-auto">
                {text}
            </section>
        </main>
    )
}

export default App
