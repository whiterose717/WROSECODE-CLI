import React, {useEffect, useState} from "react";
import {createRoot} from "react-dom/client";
import "./style.css";

function App(){
  const [status,setStatus]=useState({}); const [prompt,setPrompt]=useState(""); const [turns,setTurns]=useState([]);
  const refresh=()=>fetch("/v1/status").then(r=>r.json()).then(setStatus);
  useEffect(()=>{refresh(); const id=setInterval(refresh,1000); return()=>clearInterval(id)},[]);
  async function run(){const text=prompt;setPrompt("");setTurns(v=>[...v,["YOU",text]]);const r=await fetch("/v1/chat",{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({prompt:text})});const j=await r.json();setTurns(v=>[...v,["WROSE",j.answer||j.error]]);refresh()}
  return <><header>╦ ╦╦═╗╔═╗╔═╗╔═╗ WROSECODE</header><main>
    <section><h3>Transcript</h3><pre>{turns.map(([r,t])=>`${r} ${t}`).join("\n\n")}</pre><textarea value={prompt} onChange={e=>setPrompt(e.target.value)}/><button onClick={run}>Run</button></section>
    <section><h3>Token Dashboard</h3><pre>{JSON.stringify(status.metrics,null,2)}</pre></section>
    <section><h3>Subagent Tree</h3><pre>root · {status.model}\n└─ {status.mode}</pre></section>
    <section><h3>Tool Timeline</h3><pre>Live events are summarized in session metrics.</pre></section>
  </main></>
}
createRoot(document.getElementById("root")).render(<App/>);
