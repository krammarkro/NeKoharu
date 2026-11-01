'use client'

import { Dices } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import {
  PreferencePage,
  PreferenceRow,
  PreferenceSection,
} from '@/components/preferences/PreferenceFields'
import { call } from '@/lib/backend'
import { useForkPreferences } from '@/lib/forkPreferences'
import { commands } from '@koharu/bridge/protocol'
import { Button } from '@koharu/ui/components/button'
import { Input } from '@koharu/ui/components/input'
import { Switch } from '@koharu/ui/components/switch'

/** Matches the native limit, so the field never holds more than is kept. */
const NAME_LIMIT = 32

export function FeaturesPreferences() {
  const { t } = useTranslation()
  const preferences = useForkPreferences((state) => state.preferences)
  const setPreference = useForkPreferences((state) => state.setPreference)

  return (
    <PreferencePage
      title={t('settings.features.title')}
      description={t('settings.features.description')}
    >
      <PreferenceSection title={t('settings.features.pages')}>
        <PreferenceRow
          title={t('settings.features.mergeDeleteOriginals')}
          description={t('settings.features.mergeDeleteOriginalsDescription')}
        >
          <div className='flex h-8 items-center justify-end'>
            <Switch
              aria-label={t('settings.features.mergeDeleteOriginals')}
              checked={preferences.mergeDeleteOriginals}
              onCheckedChange={(mergeDeleteOriginals) =>
                setPreference('mergeDeleteOriginals', mergeDeleteOriginals)
              }
            />
          </div>
        </PreferenceRow>
        <PreferenceRow
          title={t('settings.features.showPageNumbers')}
          description={t('settings.features.showPageNumbersDescription')}
        >
          <div className='flex h-8 items-center justify-end'>
            <Switch
              aria-label={t('settings.features.showPageNumbers')}
              checked={preferences.showPageNumbers}
              onCheckedChange={(showPageNumbers) =>
                setPreference('showPageNumbers', showPageNumbers)
              }
            />
          </div>
        </PreferenceRow>
      </PreferenceSection>
      <PreferenceSection title={t('settings.features.collaboration')}>
        <PreferenceRow
          title={t('settings.features.displayName')}
          description={t('settings.features.displayNameDescription')}
        >
          <DisplayNameField />
        </PreferenceRow>
      </PreferenceSection>
    </PreferencePage>
  )
}

/** The name shown to the other participants of a session. It commits on blur
 * or Enter; a blank name comes back as a generated one. */
function DisplayNameField() {
  const { t } = useTranslation()
  const [saved, setSaved] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  // Escape blurs the field too; the blur that follows must not commit.
  const canceled = useRef(false)

  useEffect(() => {
    void call(commands.getSessionName)
      .then((name) => {
        setSaved(name)
        setDraft(name)
      })
      .catch(() => undefined)
  }, [])

  const commit = (name: string) => {
    if (saved === null || name.trim() === saved) {
      setDraft(saved ?? '')
      return
    }
    void call(commands.setSessionName, name)
      .then((name) => {
        setSaved(name)
        setDraft(name)
      })
      .catch(() => setDraft(saved))
  }

  return (
    <div className='flex items-center gap-1'>
      <Input
        value={draft}
        maxLength={NAME_LIMIT}
        aria-label={t('settings.features.displayName')}
        className='h-8 min-w-0 flex-1 text-[12px] text-foreground'
        disabled={saved === null}
        onChange={(event) => setDraft(event.currentTarget.value)}
        onBlur={() => {
          if (canceled.current) canceled.current = false
          else commit(draft)
        }}
        onKeyDown={(event) => {
          if (event.key === 'Enter') event.currentTarget.blur()
          if (event.key === 'Escape') {
            canceled.current = true
            setDraft(saved ?? '')
            event.currentTarget.blur()
          }
        }}
      />
      <Button
        type='button'
        variant='ghost'
        size='icon-sm'
        aria-label={t('settings.features.randomName')}
        title={t('settings.features.randomName')}
        disabled={saved === null}
        onClick={() => commit('')}
      >
        <Dices className='size-4' />
      </Button>
    </div>
  )
}
