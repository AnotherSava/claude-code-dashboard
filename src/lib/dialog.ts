import type { DialogEntry } from './types'

const PERMISSION_SEEKING = ['want me to', 'shall i', 'should i', 'do you want']

function stripTrailingOptions(text: string): string {
  const trimmed = text.replace(/\s+$/, '')
  if (!trimmed.endsWith(')')) return trimmed
  const openIdx = trimmed.lastIndexOf('(')
  if (openIdx === -1) return trimmed
  const before = trimmed.slice(0, openIdx).replace(/\s+$/, '')
  return before.endsWith('?') ? before : trimmed
}

function hasPermissionSeekingQuestion(text: string): boolean {
  const lower = text.toLowerCase()
  return PERMISSION_SEEKING.some((phrase) => {
    const idx = lower.indexOf(phrase)
    return idx !== -1 && lower.slice(idx + phrase.length).includes('?')
  })
}

function isAQuestion(text: string): boolean {
  if (stripTrailingOptions(text).endsWith('?')) return true
  return hasPermissionSeekingQuestion(text)
}

// Short confirmations / selections that are almost always replies, not new
// tasks — e.g. "y" to a skill's "proceed?" prompt that the JSONL transcript
// doesn't carry (the question lives in a tool result, not assistant text).
const SHORT_REPLY = /^(y|n|yes|no|ok|okay|sure|go|continue|proceed|done|skip|stop|all|none|both|some|\d+([\s,]+\d+)*)$/i

function isShortReply(text: string): boolean {
  return SHORT_REPLY.test(text.trim())
}

export function isTaskBoundary(dialog: DialogEntry[], idx: number): boolean {
  const entry = dialog[idx]
  if (!entry || entry.role !== 'user') return false
  if (isShortReply(entry.text)) return false
  if (idx === 0) return true
  const prev = dialog[idx - 1]
  if (prev.status === 'awaiting') return false
  if (prev.role === 'assistant' && isAQuestion(prev.text)) return false
  return true
}
