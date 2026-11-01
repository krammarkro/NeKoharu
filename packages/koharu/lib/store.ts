'use client'

import { create } from 'zustand'

import type {
  CanvasState,
  Download,
  EntityId,
  Job,
  Model,
  Frame,
  Preferences,
  SessionInfo,
  Stage,
  StartupState,
  ModelResources,
  PageSize,
} from '@koharu/bridge/protocol'
import { toast } from '@koharu/ui/components/toast'

export type CanvasTool =
  | 'select'
  | 'text'
  | 'ocr'
  | 'ocr_clean'
  | 'draw'
  | 'eraser'
  | 'color_picker'
  | 'remove'
  | 'pan'
export const MIN_BRUSH_DIAMETER = 1
export function maxBrushDiameter(size: PageSize): number {
  // Assume that for all sizes below 2048 width, 128 is sufficient.
  // For anything above, scale by percentage greater than 2048 width.
  return Math.round(Math.max(size.width / 2048, 1.0) * 128)
}

export function isBrushTool(tool: CanvasTool): boolean {
  return tool === 'draw' || tool === 'eraser' || tool === 'remove'
}

/** A joined project stops accepting edits the moment its host is gone. */
export function sessionReadOnly(session: SessionInfo): boolean {
  return session.role === 'joined' && !session.online
}

export interface CanvasBrush {
  diameter: number
  color: string
}
export type InspectorSection = 'copy' | 'type' | 'layers'
export type SidebarPanel = 'properties' | 'agent' | 'find'
export type ShortcutAction = CanvasTool | 'fit' | 'previous_page' | 'next_page'
export type Shortcuts = Record<ShortcutAction, string>
export type PipelineScope = 'page' | 'selected-pages' | 'project'
export const pipelineStages: readonly Stage[] = ['detection', 'ocr', 'translation', 'inpainting']

interface KoharuStore {
  initialized: boolean
  preferences: Preferences | null
  translationModels: Model[]
  resources: ModelResources | null
  jobs: Record<string, Job>
  downloads: Record<string, Download>
  camera: { zoom: number; translation: [number, number]; fitted: boolean }
  canvasPage: EntityId | null
  canvasRevision: number | null
  canvasGeneration: number
  canvasSize: [number, number]
  fitCanvasRequest: number
  layerFrames: Record<EntityId, Frame>
  layerFontSizes: Record<EntityId, number>
  selectedLayers: EntityId[]
  copiedTextLayers: EntityId[]
  selectedPages: EntityId[]
  tool: CanvasTool
  brush: CanvasBrush
  inspector: InspectorSection
  processingScope: PipelineScope
  processingStages: Stage[]
  settingsOpen: boolean
  sidebarPanel: SidebarPanel
  shortcuts: Shortcuts
  session: SessionInfo
  selectPages: (pages: EntityId[]) => void
  showInspector: (section: InspectorSection) => void
  setProcessingScope: (scope: PipelineScope) => void
  setProcessingStages: (stages: Stage[]) => void
  setSettingsOpen: (open: boolean) => void
  setSidebarPanel: (panel: SidebarPanel) => void
  selectLayers: (layers: EntityId[]) => void
  setCopiedTextLayers: (layers: EntityId[]) => void
  setTool: (tool: CanvasTool, cleanupLayer?: EntityId) => void
  setBrush: (brush: CanvasBrush) => void
  setShortcut: (action: ShortcutAction, key: string) => void
  setSession: (session: SessionInfo) => void
  requestCanvasFit: () => void
  dismissJob: (id: string) => void
  dismissDownload: (id: number) => void
}

export const defaultShortcuts: Shortcuts = {
  select: 'v',
  text: 't',
  ocr: 'o',
  ocr_clean: 'k',
  draw: 'b',
  eraser: 'e',
  color_picker: 'i',
  remove: 'j',
  pan: 'h',
  fit: '0',
  previous_page: 'a',
  next_page: 'd',
}

export const useKoharuStore = create<KoharuStore>()((set) => ({
  initialized: false,
  preferences: null,
  translationModels: [],
  resources: null,
  jobs: {},
  downloads: {},
  camera: { zoom: 1, translation: [0, 0], fitted: true },
  canvasPage: null,
  canvasRevision: null,
  canvasGeneration: 0,
  canvasSize: [0, 0],
  fitCanvasRequest: 0,
  layerFrames: {},
  layerFontSizes: {},
  selectedLayers: [],
  copiedTextLayers: [],
  selectedPages: [],
  tool: 'select',
  brush: { diameter: 48, color: '#FFFFFF' },
  inspector: 'copy',
  processingScope: 'selected-pages',
  processingStages: [...pipelineStages],
  settingsOpen: false,
  sidebarPanel: 'properties',
  shortcuts: defaultShortcuts,
  session: { role: null, link: null, online: false, members: [] },
  selectPages: (selectedPages) => set({ selectedPages: [...new Set(selectedPages)] }),
  showInspector: (inspector) => set({ inspector }),
  setProcessingScope: (processingScope) => set({ processingScope }),
  setProcessingStages: (processingStages) => set({ processingStages }),
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),
  setSidebarPanel: (sidebarPanel) => set({ sidebarPanel }),
  selectLayers: (selectedLayers) => set({ selectedLayers: [...new Set(selectedLayers)] }),
  setCopiedTextLayers: (copiedTextLayers) => set({ copiedTextLayers: [...copiedTextLayers] }),
  setTool: (tool, cleanupLayer) =>
    set((state) => ({
      tool,
      ...(cleanupLayer && (tool === 'draw' || tool === 'eraser')
        ? { selectedLayers: [cleanupLayer] }
        : {}),
    })),
  setBrush: (brush) => set({ brush }),
  setShortcut: (action, key) =>
    set((state) => ({
      shortcuts: { ...state.shortcuts, [action]: key.toLowerCase().slice(0, 1) },
    })),
  setSession: (session) => set({ session }),
  requestCanvasFit: () => set((state) => ({ fitCanvasRequest: state.fitCanvasRequest + 1 })),
  dismissJob: (id) =>
    set((state) => {
      const jobs = { ...state.jobs }
      delete jobs[id]
      return { jobs }
    }),
  dismissDownload: (id) =>
    set((state) => {
      const downloads = { ...state.downloads }
      delete downloads[String(id)]
      return { downloads }
    }),
}))

export function receiveStartupState(state: StartupState): void {
  useKoharuStore.setState({
    initialized: true,
    preferences: state.preferences,
    jobs: byId(state.jobs),
    ...canvasSnapshot(state.canvas),
  })
}

export function receiveCanvas(canvas: CanvasState): void {
  useKoharuStore.setState(canvasSnapshot(canvas))
}

export function receiveJob(job: Job): void {
  useKoharuStore.setState((state) => ({ jobs: { ...state.jobs, [job.id]: job } }))
}

export function receiveDownload(download: Download): void {
  useKoharuStore.setState((state) => {
    const downloads = { ...state.downloads }
    if (download.state === 'finished' || download.state === 'cancelled')
      delete downloads[String(download.id)]
    else downloads[String(download.id)] = download
    return { downloads }
  })
}

export function receivePreferences(preferences: Preferences): void {
  useKoharuStore.setState({ preferences })
}

export function receiveTranslationModels(translationModels: Model[]): void {
  useKoharuStore.setState({ translationModels })
}

export function receiveResources(resources: ModelResources): void {
  useKoharuStore.setState({ resources })
}

export function receiveSession(session: SessionInfo): void {
  useKoharuStore.setState({ session })
}

export function receiveError(message: string): void {
  toast.add({ type: 'error', title: 'Could not complete that action', description: message })
}

function canvasSnapshot(canvas: CanvasState) {
  return {
    canvasPage: canvas.page,
    canvasRevision: canvas.revision,
    canvasGeneration: canvas.generation,
    canvasSize: canvas.size,
    layerFrames: Object.fromEntries(
      canvas.element_frames.map(({ element, frame }) => [element, frame]),
    ),
    layerFontSizes: Object.fromEntries(
      canvas.text_sizes.map(({ element, font_size }) => [element, font_size]),
    ),
  }
}

function byId<T extends { id: string | number }>(items: T[]): Record<string, T> {
  return Object.fromEntries(items.map((item) => [String(item.id), item]))
}
