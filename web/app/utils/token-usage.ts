import type {TokenUsage} from './api.ts'

export function tokenUsageSummary(usage:TokenUsage|null|undefined,context:number|null|undefined,limit?:number|null) {
  return {
    total:usage?.total_tokens ?? null,
    context:context ?? null,
    percent:context!=null && limit!=null && limit>0 ? Math.round(context/limit*100) : null,
    partial:usage?.partial ?? true,
  }
}
