export function linkTarget(href:unknown):'_self'|'_blank' {
  try {return typeof href==='string' && new URL(href,location.href).origin!==location.origin?'_blank':'_self'}
  catch {return '_self'}
}
