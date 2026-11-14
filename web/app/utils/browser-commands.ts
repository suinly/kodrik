type Reply={request_id:string;result?:unknown;error?:string}
// Keep polling during confirmation/execution; retry delivery, never the browser action.
export function createBrowserCommandPoller(options:{
  poll:()=>Promise<{request_id:string|null;command?:Record<string,unknown>|null}>;
  execute:(command:Record<string,unknown>,id:string)=>Promise<unknown>;
  cancel?:(id:string)=>void;
  complete:(reply:Reply)=>Promise<unknown>;
}) {
  let stopped=false,polling=false
  const requests=new Map<string,{reply?:Reply}>()
  return {
    async tick(){
      if(stopped || polling) return
      polling=true
      try {
        const {request_id,command}=await options.poll()
        if(stopped) return
        for(const [id,entry] of requests) if(id!==request_id && !entry.reply) options.cancel?.(id)
        if(!request_id || !command) return
        let entry=requests.get(request_id)
        if(!entry) {
          entry={};requests.set(request_id,entry)
          const current=entry
          void (async()=>{
            try {current.reply={request_id,result:await options.execute(command,request_id)}}
            catch(error){current.reply={request_id,error:error instanceof Error?error.message:String(error)}}
            if(!stopped) {try {await options.complete(current.reply)} catch {/* Retry delivery on heartbeat. */}}
          })()
        } else if(entry.reply) await options.complete(entry.reply)
      } catch { /* Connection loss must not trigger another execution. */ }
      finally {polling=false}
    },
    stop(){stopped=true;for(const [id,entry] of requests) if(!entry.reply) options.cancel?.(id)},
  }
}
