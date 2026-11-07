export type ChatActivity = {busy:boolean;unread:boolean}

export function faviconHref(icon:string,activity:ChatActivity,frame:number,reducedMotion=false) {
  const radius=reducedMotion ? 35 : [35,39,44,48,44,39][frame%6]!
  const markers=(activity.busy ? `<circle cx="72" cy="72" r="${radius}" fill="#89b4fa"/>` : '')+
    (activity.unread ? '<circle cx="440" cy="72" r="42" fill="#f38ba8"/>' : '')
  return `data:image/svg+xml,${encodeURIComponent(icon.replace('</svg>',`${markers}</svg>`))}`
}
