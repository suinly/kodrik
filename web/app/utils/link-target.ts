export function linkTarget(href:unknown):'_self'|'_blank' {
  try {return typeof href==='string' && new URL(href,location.href).origin!==location.origin?'_blank':'_self'}
  catch {return '_self'}
}

export function resolveLinkProps(props:Record<string,unknown>,baseUrl:string):Record<string,unknown> {
  function resolve(value:unknown,key=''):unknown {
    if(['data','state','value','modelValue','defaultValue'].includes(key)) return value
    if(typeof value==='string' && ['href','to','src'].includes(key) && !value.startsWith('#')) return new URL(value,baseUrl).href
    if(Array.isArray(value)) return value.map(item=>resolve(item))
    if(value && typeof value==='object') return Object.fromEntries(Object.entries(value).map(([name,item])=>[name,resolve(item,name)]))
    return value
  }
  return resolve(props) as Record<string,unknown>
}
