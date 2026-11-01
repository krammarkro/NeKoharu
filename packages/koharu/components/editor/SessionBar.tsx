'use client'

import type { TFunction } from 'i18next'
import { Check, Copy, Link2, LoaderCircle, UserPlus, Users } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { call } from '@/lib/backend'
import { pageKey, pagesKey, projectKey, refresh, usePage, usePages } from '@/lib/queries'
import { sessionReadOnly, useKoharuStore } from '@/lib/store'
import { commands, type SessionInfo, type SessionMember } from '@koharu/bridge/protocol'
import { Badge } from '@koharu/ui/components/badge'
import { Button } from '@koharu/ui/components/button'
import { Input } from '@koharu/ui/components/input'
import { Popover, PopoverContent, PopoverTrigger } from '@koharu/ui/components/popover'
import { toast } from '@koharu/ui/components/toast'
import { cn } from '@koharu/ui/lib/utils'

const NOTICE_DURATION_MS = 4000
const LINK_COPIED_MS = 2000

const MEMBER_COLORS = [
  'bg-sky-500',
  'bg-amber-500',
  'bg-violet-500',
  'bg-rose-500',
  'bg-teal-500',
  'bg-lime-600',
]

/** The name a participant goes by, or their role and number when their
 * machine did not say. */
export function memberName(t: TFunction, member: SessionMember): string {
  if (member.name) return member.name
  return member.host ? t('session.host') : t('session.guest', { number: member.peer })
}

/** A participant's initial on their color; the color follows the peer id, so
 * it matches across the rail and the share menu. */
export function MemberAvatar({ member, className }: { member: SessionMember; className?: string }) {
  const { t } = useTranslation()
  const name = memberName(t, member)
  return (
    <span
      aria-hidden='true'
      className={cn(
        'grid size-4 shrink-0 place-items-center rounded-full text-[8px] leading-none font-semibold text-white uppercase',
        MEMBER_COLORS[member.peer % MEMBER_COLORS.length],
        className,
      )}
    >
      {name.trim().charAt(0)}
    </span>
  )
}

function statusKey(session: SessionInfo): string | null {
  if (session.role === 'hosting') return session.online ? 'session.hosting' : 'session.linkOffline'
  if (session.role === 'joined') return session.online ? 'session.joined' : 'session.hostOffline'
  return null
}

/** Announces each session status change over the canvas, and to a host each
 * guest that joins, then fades out; the share menu keeps the status, members,
 * and actions afterwards. */
export function SessionNotice() {
  const { t } = useTranslation()
  const status = useKoharuStore((state) => statusKey(state.session))
  const role = useKoharuStore((state) => state.session.role)
  const members = useKoharuStore((state) => state.session.members)
  const warning = useKoharuStore(
    (state) =>
      sessionReadOnly(state.session) || (state.session.role === 'hosting' && !state.session.online),
  )
  const [visible, setVisible] = useState(false)
  // The latest announcement wins: a status change clears a pending arrival.
  const [joined, setJoined] = useState<string | null>(null)
  const [announced, setAnnounced] = useState(0)
  // Who was already here; null until first seen, so remounting mid-session
  // does not announce everyone again.
  const seen = useRef<Set<number> | null>(null)

  useEffect(() => {
    if (!status) return
    setJoined(null)
    setAnnounced((count) => count + 1)
  }, [status])

  useEffect(() => {
    const arrivals =
      role === 'hosting'
        ? members.filter((member) => !member.you && seen.current?.has(member.peer) === false)
        : []
    seen.current = new Set(members.map((member) => member.peer))
    if (arrivals.length === 0) return
    setJoined(arrivals.map((member) => memberName(t, member)).join(', '))
    setAnnounced((count) => count + 1)
  }, [members, role, t])

  useEffect(() => {
    if (announced === 0) return
    setVisible(true)
    const timer = window.setTimeout(() => setVisible(false), NOTICE_DURATION_MS)
    return () => window.clearTimeout(timer)
  }, [announced])

  if (!status) return null

  const Icon = joined ? UserPlus : role === 'hosting' ? Link2 : Users
  const alert = warning && !joined

  return (
    <div
      role='status'
      aria-hidden={!visible}
      className={cn(
        'pointer-events-none absolute top-3 left-1/2 z-30 flex max-w-[min(72vw,560px)] -translate-x-1/2 items-center gap-2 rounded-full border border-border bg-[var(--surface-floating)] px-3 py-1.5 text-[11px] shadow-[var(--shadow-toolrail)] transition-opacity duration-500',
        visible ? 'opacity-100' : 'opacity-0',
      )}
    >
      <Icon className='size-3.5 shrink-0 text-muted-foreground' />
      <span className={cn('truncate', alert ? 'font-medium text-destructive' : 'text-foreground')}>
        {joined ? t('session.memberJoined', { name: joined }) : t(status)}
      </span>
    </div>
  )
}

/** The command bar's share control: starts sharing, then holds the session's
 * link and the actions that end it. */
export function SessionMenu() {
  const { t } = useTranslation()
  const page = usePage().data
  const pages = usePages().data
  const session = useKoharuStore((state) => state.session)
  const [busy, setBusy] = useState(false)
  const [copied, setCopied] = useState(false)
  const [open, setOpen] = useState(false)
  const [invite, setInvite] = useState('')
  const [linkCopied, setLinkCopied] = useState(false)

  // The confirmation keeps its own clock: toasts pause while the window is
  // unfocused, which is exactly when a host is pasting the link elsewhere.
  useEffect(() => {
    if (!linkCopied) return
    const timer = window.setTimeout(() => setLinkCopied(false), LINK_COPIED_MS)
    return () => window.clearTimeout(timer)
  }, [linkCopied])

  const run = (operation: () => Promise<unknown>) => {
    setBusy(true)
    void operation()
      .catch(() => undefined)
      .finally(() => setBusy(false))
  }

  const confirmation = (
    <span
      role='status'
      aria-hidden={!linkCopied}
      className={cn(
        'pointer-events-none absolute top-full right-0 z-40 mt-1.5 rounded-md bg-foreground px-2 py-1 text-[10px] whitespace-nowrap text-background shadow-md transition-opacity duration-300',
        linkCopied ? 'opacity-100' : 'opacity-0',
      )}
    >
      {t('session.linkCopied')}
    </span>
  )

  const triggerClass =
    'relative grid size-7 shrink-0 place-items-center rounded-lg text-muted-foreground transition-colors outline-none hover:bg-primary/10 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/25 disabled:pointer-events-none disabled:opacity-40 data-open:bg-primary/10 data-open:text-foreground'

  const status = statusKey(session)
  if (!status) {
    // Opening a tunnel takes a few seconds; the link lands on the clipboard
    // as soon as it exists, or the menu opens so it can be copied by hand.
    const share = () =>
      run(async () => {
        const { link } = await call(commands.startSession)
        if (!link) return
        try {
          await navigator.clipboard.writeText(link)
          setLinkCopied(true)
        } catch {
          setOpen(true)
        }
      })

    return (
      <div className='relative shrink-0'>
        <button
          type='button'
          aria-label={t('session.share')}
          title={`${t('session.share')} (${t('common.experimental')})`}
          className={triggerClass}
          disabled={!page}
          aria-busy={busy}
          onClick={() => {
            if (!busy) share()
          }}
        >
          {busy ? (
            <LoaderCircle aria-hidden='true' className='size-4 animate-spin text-primary' />
          ) : (
            <Link2 className='size-4' />
          )}
        </button>
        {confirmation}
      </div>
    )
  }

  const hosting = session.role === 'hosting'
  const warning = !session.online

  const copy = async () => {
    if (!session.link) return
    try {
      await navigator.clipboard.writeText(session.link)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    } catch {
      toast.add({ type: 'error', title: t('session.copyFailed') })
    }
  }

  const Icon = hosting ? Link2 : Users

  return (
    <div className='relative shrink-0'>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger
          type='button'
          aria-label={t(status)}
          title={t(status)}
          className={triggerClass}
        >
          <Icon className={cn('size-4', !warning && 'text-primary')} />
          <span
            aria-hidden='true'
            className={cn(
              'absolute top-1 right-1 size-1.5 rounded-full ring-2 ring-[var(--surface-toolbar)]',
              warning ? 'bg-destructive' : 'bg-emerald-500',
            )}
          />
        </PopoverTrigger>
        <PopoverContent align='end' className='w-80 gap-2 p-2 text-[11px]'>
          <div className='flex items-center gap-2 px-1 pt-0.5'>
            <Icon className='size-3.5 shrink-0 text-muted-foreground' />
            <span
              className={cn(
                'truncate font-medium',
                warning ? 'text-destructive' : 'text-foreground',
              )}
            >
              {t(status)}
            </span>
            <Badge variant='outline' className='ml-auto h-4 px-1.5 text-[9px] font-normal'>
              {t('common.experimental')}
            </Badge>
          </div>
          {session.members.length > 0 && (
            <section className='flex flex-col gap-0.5'>
              <h3 className='px-1 text-[10px] font-normal text-muted-foreground'>
                {t('session.members')}
              </h3>
              <ul className='flex flex-col'>
                {session.members.map((member) => {
                  const index = member.page
                    ? (pages?.findIndex((entry) => entry.id === member.page) ?? -1)
                    : -1
                  return (
                    <li key={member.peer} className='flex items-center gap-2 rounded-md px-1 py-1'>
                      <MemberAvatar member={member} />
                      <span className='min-w-0 truncate text-foreground'>
                        {memberName(t, member)}
                        {member.you && (
                          <span className='text-muted-foreground'> ({t('session.you')})</span>
                        )}
                      </span>
                      {member.host && (
                        <Badge variant='outline' className='h-4 px-1.5 text-[9px] font-normal'>
                          {t('session.host')}
                        </Badge>
                      )}
                      <span className='ml-auto shrink-0 text-muted-foreground tabular-nums'>
                        {index >= 0
                          ? t('session.onPage', { number: index + 1 })
                          : t('session.noPage')}
                      </span>
                    </li>
                  )
                })}
              </ul>
            </section>
          )}
          {hosting && session.link && (
            <div
              className='truncate rounded-md border border-border bg-muted/40 px-2 py-1.5 font-mono text-[10px] text-muted-foreground select-all'
              title={session.link}
            >
              {session.link}
            </div>
          )}
          {/* A host that restarted has a new link; joining it keeps this replica
            until the new link delivers the project. */}
          {!hosting && warning && (
            <form
              className='flex items-center gap-1'
              onSubmit={(event) => {
                event.preventDefault()
                const link = invite.trim()
                if (!link) return
                run(() =>
                  call(commands.joinSession, link)
                    .then(() => refresh(projectKey, pagesKey, pageKey))
                    .then(() => {
                      setInvite('')
                      setOpen(false)
                    }),
                )
              }}
            >
              <Input
                value={invite}
                onChange={(event) => setInvite(event.target.value)}
                placeholder={t('session.joinPlaceholder')}
                aria-label={t('session.join')}
                className='h-7 min-w-0 flex-1 text-[11px]'
                disabled={busy}
              />
              <Button
                type='submit'
                variant='ghost'
                size='sm'
                className='h-7 shrink-0 text-[11px]'
                disabled={!invite.trim() || busy}
                aria-busy={busy}
              >
                {t('session.joinAction')}
              </Button>
            </form>
          )}
          <div className='flex items-center justify-end gap-1'>
            {hosting ? (
              <>
                <Button
                  type='button'
                  variant='ghost'
                  size='sm'
                  className='h-7 gap-1.5 text-[11px]'
                  disabled={!session.link || busy}
                  onClick={() => void copy().catch(() => undefined)}
                >
                  {copied ? <Check className='size-3.5' /> : <Copy className='size-3.5' />}
                  {copied ? t('session.copied') : t('session.copy')}
                </Button>
                <Button
                  type='button'
                  variant='ghost'
                  size='sm'
                  className='h-7 text-[11px] text-destructive hover:bg-destructive/10 hover:text-destructive'
                  disabled={busy}
                  onClick={() => {
                    setOpen(false)
                    run(() => call(commands.stopSession))
                  }}
                >
                  {t('session.stopShare')}
                </Button>
              </>
            ) : (
              <Button
                type='button'
                variant='ghost'
                size='sm'
                className='h-7 text-[11px]'
                disabled={busy}
                onClick={() => {
                  setOpen(false)
                  run(() =>
                    call(commands.leaveSession).then(() => refresh(projectKey, pagesKey, pageKey)),
                  )
                }}
              >
                {t('session.leave')}
              </Button>
            )}
          </div>
        </PopoverContent>
      </Popover>
      {confirmation}
    </div>
  )
}
