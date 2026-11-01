'use client'

import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { call } from '@/lib/backend'
import { cssFrame } from '@/lib/geometry'
import { pageKey, projectKey, refresh } from '@/lib/queries'
import { useKoharuStore } from '@/lib/store'
import { commands, type Frame, type Layer } from '@koharu/bridge/protocol'

type TextLayer = Extract<Layer, { type: 'text' }>

const textAlign = { Start: 'start', Center: 'center', End: 'end', Justify: 'justify' } as const

/**
 * Edits a text layer's translation in place over its frame. The draft stays in the browser and
 * commits once when editing ends, so the whole edit is a single undo step.
 */
export function TextEditOverlay({
  layer,
  frame,
  onClose,
}: {
  layer: TextLayer
  frame: Frame
  onClose: () => void
}) {
  const { t } = useTranslation()
  const camera = useKoharuStore((state) => state.camera)
  const fittedSize = useKoharuStore((state) => state.layerFontSizes[layer.id])
  const initial = layer.content.translation?.text ?? ''
  const [text, setText] = useState(initial)
  const input = useRef<HTMLTextAreaElement>(null)
  const finished = useRef(false)

  useEffect(() => {
    const element = input.current
    if (!element) return
    element.focus()
    element.select()
  }, [])

  const finish = (commit: boolean) => {
    if (finished.current) return
    finished.current = true
    onClose()
    if (!commit || text === initial) return
    void call(commands.setTranslation, layer.id, text.trim() ? text : null)
      .then(() => refresh(projectKey, pageKey))
      .catch(() => undefined)
  }

  const typography = layer.typography
  const position = cssFrame(frame, camera)
  const scale = camera.zoom / window.devicePixelRatio
  const fontSize = Math.min(Math.max((fittedSize ?? typography?.size ?? 24) * scale, 11), 72)
  const vertical = typography?.writing_mode === 'Vertical'

  return (
    <textarea
      ref={input}
      data-canvas-control
      data-testid='canvas-text-editor'
      aria-label={t('layers.translationLabel', { name: layer.content.source?.text ?? '' })}
      placeholder={layer.content.source?.text}
      spellCheck={false}
      value={text}
      className='absolute z-20 box-border resize-none overflow-auto rounded-[2px] border border-[var(--canvas-selection)] bg-background/95 p-1 text-foreground shadow-md outline-none'
      style={{
        left: position.left,
        top: position.top,
        width: Math.max(position.width, 48),
        height: Math.max(position.height, fontSize * 1.6),
        transform: `rotate(${position.angle}deg)`,
        transformOrigin: '50% 50%',
        fontSize,
        lineHeight: typography?.line_spacing ?? 1.2,
        fontFamily: typography?.preferred_font
          ? `"${typography.preferred_font}", sans-serif`
          : undefined,
        fontWeight: typography?.font_weight ?? undefined,
        fontStyle: typography?.font_style ?? undefined,
        textAlign: textAlign[typography?.alignment ?? 'Center'],
        writingMode: vertical ? 'vertical-rl' : undefined,
      }}
      onChange={(event) => setText(event.currentTarget.value)}
      onBlur={() => finish(true)}
      onPointerDown={(event) => event.stopPropagation()}
      onWheel={(event) => event.stopPropagation()}
      onKeyDown={(event) => {
        if (event.nativeEvent.isComposing) return
        if (event.key === 'Escape') {
          event.preventDefault()
          finish(false)
        } else if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
          event.preventDefault()
          finish(true)
        }
      }}
    />
  )
}
