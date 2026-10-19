/**
 * Cloud Sync preferences tab.
 *
 * Two states:
 *  - Not signed in: a wizard with two steps — authenticate (PAT or GitHub
 *    device flow), then name the repo. The backend decides who wins the
 *    first sync (repo missing → push local; repo with data → pull and
 *    overwrite local; repo empty → push local).
 *  - Signed in: account summary, last sync times, and manual pull / push /
 *    sign-out actions. Everything else (startup pull, debounced auto push)
 *    happens in the Rust backend without UI involvement.
 */

import {
  Alert,
  Button,
  Group,
  SegmentedControl,
  Stack,
  Text,
  TextInput,
} from '@mantine/core'
import { IconCloud, IconExternalLink } from '@tabler/icons-react'
import { actions } from '@renderer/core/agent'
import {
  showErrorNotification,
  showSuccessNotification,
} from '@renderer/core/notify'
import { githubDeviceClientId } from '@common/constants'
import useI18n from '@renderer/models/useI18n'
import { useCallback, useEffect, useRef, useState } from 'react'

const DEFAULT_REPO = 'switchhosts-backup'

type PlatformId = 'github' | 'gitee'
type AuthMethodId = 'pat' | 'device'

interface SyncStatus {
  loggedIn: boolean
  platform?: string
  auth_method?: string
  login?: string
  repo?: string
  has_device_client_id?: boolean
  last_push_at?: string | null
  last_pull_at?: string | null
}

interface DeviceLoginInfo {
  deviceCode: string
  userCode: string
  verificationUri: string
  interval: number
}

const Ved = 'var(--mantine-spacing-md)'
const rowGap = '24px'

function formatStamp(stamp: string | null | undefined, neverText: string): string {
  if (!stamp) return neverText
  const date = new Date(stamp)
  return Number.isNaN(date.getTime()) ? stamp : date.toLocaleString()
}

/** Copy text to the clipboard. Prefers the async web API; falls back to a
 *  hidden textarea + execCommand when the webview lacks a secure-context
 *  clipboard (older webviews, or clipboard permission withheld). */
async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    // fall through to the legacy path below
  }
  try {
    const area = document.createElement('textarea')
    area.value = text
    area.style.position = 'fixed'
    area.style.opacity = '0'
    document.body.appendChild(area)
    area.select()
    const ok = document.execCommand('copy')
    document.body.removeChild(area)
    return ok
  } catch {
    return false
  }
}

const CloudSync = () => {
  const { lang, i18n } = useI18n()

  const [account, setAccount] = useState<SyncStatus | null>(null)
  const [loaded, setLoaded] = useState(false)

  // Wizard state
  const [platform, setPlatform] = useState<PlatformId>('github')
  const [authMethod, setAuthMethod] = useState<AuthMethodId>('pat')
  const [token, setToken] = useState('')
  const [deviceClientId, setDeviceClientId] = useState(githubDeviceClientId)
  const [deviceInfo, setDeviceInfo] = useState<DeviceLoginInfo | null>(null)
  const [deviceError, setDeviceError] = useState('')
  const [copied, setCopied] = useState<'' | 'code' | 'link'>('')
  const [repo, setRepo] = useState(DEFAULT_REPO)
  const [verifiedLogin, setVerifiedLogin] = useState('')
  const [step, setStep] = useState<'auth' | 'repo'>('auth')
  const [busy, setBusy] = useState(false)
  const [authError, setAuthError] = useState('')
  const [createRepoNeeded, setCreateRepoNeeded] = useState(false)
  // True when the device-flow token we obtained starts with `ghu_` — i.e. it
  // was issued by a GitHub App rather than an OAuth App. Such tokens cannot
  // create user repositories (GitHub answers 403 "Resource not accessible
  // by integration" on POST /user/repos), so we warn early instead of
  // letting the user discover it after a failed round trip.
  const [githubAppToken, setGithubAppToken] = useState(false)
  /** Scope-aware error text. `forbidden` is the interesting case: whether
   *  the user hit a fine-grained vs classic GitHub token, or a Gitee token,
   *  changes the remediation, so we branch on the selected platform and, on
   *  GitHub, on the token prefix (`github_pat_` fine-grained, `ghp_` classic).
   *  The default branch shows the generic forbidden message with the same
   *  wording family for unknown token shapes. */
  const errorText = (raw: unknown, source: { platform: PlatformId; authMethod: AuthMethodId; token?: string } = { platform, authMethod }) => {
    const code = typeof raw === 'string' ? raw : 'internal'
    const dict = lang as Record<string, string>
    if (code.startsWith('api_')) {
      const status = code.slice(4)
      return dict.sync_error_api.replace('{0}', status)
    }
    if (code === 'create_repo_not_allowed') {
      return dict.sync_error_create_repo_not_allowed
    }
    if (code === 'forbidden') {
      // Note: the local is renamed to avoid a TDZ self-reference — the outer
      // component `token` state is read on the right-hand side.
      const activeToken = (source.token ?? token).trim()
      if (source.platform === 'gitee') {
        return dict.sync_error_forbidden_gitee
      }
      if (activeToken.startsWith('github_pat_')) {
        return dict.sync_error_forbidden_github_fine
      }
      return dict.sync_error_forbidden_github_classic
    }
    const mapped = dict['sync_error_' + code]
    return mapped ?? code
  }


  const pollTimer = useRef<ReturnType<typeof setTimeout> | null>(null)

  const loadStatus = useCallback(async () => {
    try {
      const status = await actions.syncStatus()
      setAccount(status)
    } catch (e) {
      console.error('sync_status failed', e)
      setAccount({ loggedIn: false })
    } finally {
      setLoaded(true)
    }
  }, [])

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- one-shot async status fetch on mount; the setState happens after the round-trip
    loadStatus()
  }, [loadStatus])


  useEffect(() => () => cancelDevicePoll(), [])

  function cancelDevicePoll() {
    if (pollTimer.current) {
      clearTimeout(pollTimer.current)
      pollTimer.current = null
    }
  }

  const startDeviceLogin = async () => {
    setBusy(true)
    setDeviceError('')
    setGithubAppToken(false)
    try {
      const r = await actions.syncStartDeviceLogin(deviceClientId.trim())
      if (!r?.deviceCode || !r?.userCode || !r?.verificationUri) {
        throw new Error('invalid response')
      }
      setDeviceInfo({
        deviceCode: r.deviceCode,
        userCode: r.userCode,
        verificationUri: r.verificationUri,
        interval: r.interval || 5,
      })
      // Auto-open the authorization page in the default browser and stage
      // the user code in the clipboard so the user only pastes on the page.
      actions.openUrl(r.verificationUri).catch((e) => console.error(e))
      if (await copyText(r.userCode)) {
        setCopied('code')
      }
      schedulePoll(r.deviceCode, r.interval || 5)
    } catch (e) {
      setDeviceError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const schedulePoll = (deviceCode: string, interval: number) => {
    cancelDevicePoll()
    pollTimer.current = setTimeout(() => doPoll(deviceCode, interval), (interval + 1) * 1000)
  }

  const doPoll = async (deviceCode: string, interval: number) => {
    try {
      const r = await actions.syncPollDeviceLogin(deviceClientId.trim(), deviceCode)
      if (r?.kind === 'pending') {
        schedulePoll(deviceCode, r.interval || interval)
        return
      }
      if (r?.kind === 'token') {
        setVerifiedLogin(r.login || '')
        setToken(r.token || '')
        // GitHub App user tokens (ghu_) can read/write existing repos but
        // cannot create them; OAuth App tokens (gho_) can. Flag it so the
        // repo step can explain the limitation up front.
        setGithubAppToken((r.token || '').startsWith('ghu_'))
        setStep('repo')
        setDeviceInfo(null)
        return
      }
      if (r?.kind === 'expired') {
        setDeviceError(lang.cloud_sync_device_expired)
      } else if (r?.kind === 'denied') {
        setDeviceError(lang.cloud_sync_device_denied)
      } else {
        setDeviceError(errorText(r))
      }
      setDeviceInfo(null)
    } catch (e) {
      setDeviceError(errorText(e))
      setDeviceInfo(null)
    }
  }

  const verifyPat = async () => {
    if (busy || !token.trim()) return
    setBusy(true)
    setAuthError('')
    setGithubAppToken(false)
    try {
      const r = await actions.syncVerifyToken(platform, token.trim())
      if (!r?.login) throw new Error('invalid response')
      setVerifiedLogin(r.login)
      setStep('repo')
    } catch (e) {
      setAuthError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const runSetup = async () => {
    if (busy || !repo.trim()) return
    setBusy(true)
    setAuthError('')
    setCreateRepoNeeded(false)
    try {
      const r = await actions.syncSetup({
        platform,
        authMethod,
        token: token.trim(),
        login: verifiedLogin,
        repo: repo.trim(),
        deviceClientId: authMethod === 'device' ? deviceClientId.trim() : undefined,
      })
      let message = lang.cloud_sync_outcome_pushed.replace('{0}', '0')
      if (r?.outcome === 'created') {
        message = lang.cloud_sync_outcome_created.replace('{0}', String(r.hostsCount ?? 0))
      } else if (r?.outcome === 'pushed') {
        message = lang.cloud_sync_outcome_pushed.replace('{0}', String(r.hostsCount ?? 0))
      } else if (r?.outcome === 'pulled') {
        message =
          lang.cloud_sync_outcome_pulled.replace('{0}', String(r.hostsCount ?? 0)) +
          ' ' +
          lang.cloud_sync_snapshot_note
      }
      showSuccessNotification({ title: lang.cloud_sync, message })
      setStep('auth')
      setToken('')
      setVerifiedLogin('')
      await loadStatus()
    } catch (e) {
      const raw = typeof e === 'string' ? e : ''
      if (raw === 'create_repo_not_allowed') {
        // Fine-grained PATs cannot create repos; guide the user through a
        // one-click prefilled creation page, then let them retry right
        // here instead of leaving them with a wall of error text.
        setCreateRepoNeeded(true)
      } else {
        showErrorNotification({ title: lang.cloud_sync, message: errorText(e) })
      }
    } finally {
      setBusy(false)
    }
  }

  const manualPull = async () => {
    if (busy) return
    setBusy(true)
    try {
      const r = await actions.syncPull()
      if (r?.outcome === 'up_to_date') {
        showSuccessNotification({ title: lang.cloud_sync, message: lang.cloud_sync_up_to_date })
      } else if (r?.outcome === 'pulled') {
        showSuccessNotification({ title: lang.cloud_sync, message: lang.cloud_sync_pulled_ok })
      } else if (r === undefined) {
        // backend returns null when the payload is unchanged
        showSuccessNotification({ title: lang.cloud_sync, message: lang.cloud_sync_up_to_date })
      }
      await loadStatus()
    } catch (e) {
      const code = String(e)
      if (code === 'nothing_to_pull') {
        showErrorNotification({ title: lang.cloud_sync, message: lang.cloud_sync_nothing_to_pull })
      } else {
        showErrorNotification({ title: lang.cloud_sync, message: errorText(e) })
      }
    } finally {
      setBusy(false)
    }
  }

  const manualPush = async () => {
    if (busy) return
    setBusy(true)
    try {
      const r = await actions.syncPush()
      const message =
        r?.outcome === 'up_to_date' ? lang.cloud_sync_up_to_date : lang.cloud_sync_pushed_ok
      showSuccessNotification({ title: lang.cloud_sync, message })
      await loadStatus()
    } catch (e) {
      showErrorNotification({ title: lang.cloud_sync, message: errorText(e) })
    } finally {
      setBusy(false)
    }
  }

  const doLogout = async () => {
    if (busy) return
    if (!window.confirm(lang.cloud_sync_logout_confirm)) return
    setBusy(true)
    try {
      await actions.syncLogout()
      await loadStatus()
    } catch (e) {
      showErrorNotification({ title: lang.cloud_sync, message: errorText(e) })
    } finally {
      setBusy(false)
    }
  }

  if (!loaded) return null

  // ---- Logged-in view ------------------------------------------------------
  if (account?.loggedIn) {
    return (
      <Stack gap={rowGap}>
        <Group gap="8px">
          <IconCloud size={18} stroke={1.8} />
          <Text fw={500}>{lang.cloud_sync_logged_in}</Text>
        </Group>
        <Text size="md">
          {i18n.trans('cloud_sync_repo_label', [
            account.login || '',
            account.repo || '',
          ])}
        </Text>
        <Text size="md" c="dimmed">
          {account.platform || ''} · {account.auth_method || ''}
        </Text>
        <Text size="md" c="dimmed">
          {lang.cloud_sync_last_push.replace('{0}', formatStamp(account.last_push_at, lang.cloud_sync_never))}
        </Text>
        <Text size="md" c="dimmed">
          {lang.cloud_sync_last_pull.replace('{0}', formatStamp(account.last_pull_at, lang.cloud_sync_never))}
        </Text>
        <Group gap="12px" mt={Ved}>
          <Button onClick={manualPull} loading={busy} variant="outline">
            {lang.cloud_sync_pull_now}
          </Button>
          <Button onClick={manualPush} loading={busy}>
            {lang.cloud_sync_push_now}
          </Button>
          <Button onClick={doLogout} loading={busy} variant="subtle" color="red">
            {lang.cloud_sync_logout}
          </Button>
        </Group>
      </Stack>
    )
  }

  // ---- Wizard view ---------------------------------------------------------
  const deviceAuthAvailable = platform === 'github' && authMethod === 'device' && !!deviceClientId.trim()
  const canVerify =
    authMethod === 'pat' ? token.trim().length > 0 : deviceAuthAvailable && !deviceInfo

  return (
    <Stack gap={rowGap}>
      <Group gap="8px">
        <IconCloud size={18} stroke={1.8} />
        <Text size="md" c="dimmed">
          {lang.cloud_sync_desc}
        </Text>
      </Group>

      {step === 'auth' ? (
        <>
          <Stack gap="6px">
            <Text size="md" fw={500}>
              {lang.cloud_sync_platform}
            </Text>
            <SegmentedControl
              value={platform}
              onChange={(v) => {
                const next = v as PlatformId
                setPlatform(next)
                // Device authorization only exists on GitHub; fall back to
                // PAT when switching to Gitee with device selected.
                if (next === 'gitee' && authMethod === 'device') {
                  setAuthMethod('pat')
                  cancelDevicePoll()
                  setDeviceInfo(null)
                  setDeviceError('')
                }
              }}
              data={[
                { value: 'github', label: 'GitHub' },
                { value: 'gitee', label: 'Gitee' },
              ]}
            />
          </Stack>

          <Stack gap="6px">
            <Text size="md" fw={500}>
              {lang.cloud_sync_auth_method}
            </Text>
            <SegmentedControl
              value={authMethod}
              onChange={(v) => setAuthMethod(v as AuthMethodId)}
              data={[
                { value: 'pat', label: lang.cloud_sync_auth_pat },
                {
                  value: 'device',
                  label: lang.cloud_sync_auth_device,
                  disabled: platform !== 'github',
                },
              ]}
            />
          </Stack>

          {authMethod === 'pat' ? (
            <Stack gap="6px">
              <Text size="md" fw={500}>
                {lang.cloud_sync_token}
              </Text>
              <TextInput
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="ghp_… / ghp_…"
                disabled={busy}
              />
              <Text size="sm" c="dimmed">
                {lang.cloud_sync_token_help}
              </Text>
              {authError ? <Alert color="red">{authError}</Alert> : null}
              <Group gap="12px">
                <Button onClick={verifyPat} loading={busy} disabled={!canVerify}>
                  {busy ? lang.cloud_sync_verifying : lang.cloud_sync_verify}
                </Button>
              </Group>
            </Stack>
          ) : (
            <Stack gap="6px">
              {githubDeviceClientId ? null : (
                <>
                  <Text size="md" fw={500}>
                    {lang.cloud_sync_device_client_id}
                  </Text>
                  <TextInput
                    value={deviceClientId}
                    onChange={(e) => setDeviceClientId(e.target.value)}
                    placeholder="Iv1.abcdef123456"
                    disabled={busy}
                  />
                  <Text size="sm" c="dimmed">
                    {lang.cloud_sync_device_client_id_help}
                  </Text>
                </>
              )}
              {deviceError ? <Alert color="red">{deviceError}</Alert> : null}
              {deviceInfo ? (
                <Alert color="blue" title={lang.cloud_sync_device_waiting}>
                  <Stack gap="8px">
                    <Text size="sm" c="dimmed">
                      {lang.cloud_sync_code_in_clipboard}
                    </Text>
                    <Group gap="8px">
                      <Text
                        size="md"
                        ff="monospace"
                        fw={700}
                        style={{
                          fontSize: '1.15rem',
                          letterSpacing: '0.08em',
                          cursor: 'pointer',
                          userSelect: 'all',
                        }}
                        title={lang.cloud_sync_copy}
                        onClick={() => copyText(deviceInfo.userCode)}
                      >
                        {deviceInfo.userCode}
                      </Text>
                      <Button
                        size="xs"
                        variant="light"
                        onClick={async () => {
                          setCopied((await copyText(deviceInfo.userCode)) ? 'code' : '')
                        }}
                      >
                        {copied === 'code' ? lang.cloud_sync_copied : lang.cloud_sync_copy}
                      </Button>
                    </Group>
                    <Group gap="8px">
                      <Button
                        size="xs"
                        variant="light"
                        leftSection={<IconExternalLink size={14} stroke={1.8} />}
                        onClick={() =>
                          actions.openUrl(deviceInfo.verificationUri).catch((e) =>
                            console.error(e),
                          )
                        }
                      >
                        {lang.cloud_sync_open_page}
                      </Button>
                      <Button
                        size="xs"
                        variant="subtle"
                        onClick={async () => {
                          setCopied((await copyText(deviceInfo.verificationUri)) ? 'link' : '')
                        }}
                      >
                        {copied === 'link'
                          ? lang.cloud_sync_copied
                          : lang.cloud_sync_copy + ' URL'}
                      </Button>
                    </Group>
                    <Text
                      size="sm"
                      c="dimmed"
                      style={{ wordBreak: 'break-all', cursor: 'pointer' }}
                      onClick={() =>
                        actions.openUrl(deviceInfo.verificationUri).catch((e) => console.error(e))
                      }
                    >
                      {deviceInfo.verificationUri}
                    </Text>
                  </Stack>
                </Alert>
              ) : (
                <Group gap="12px">
                  <Button onClick={startDeviceLogin} loading={busy} disabled={!canVerify}>
                    {lang.cloud_sync_verify}
                  </Button>
                </Group>
              )}
            </Stack>
          )}
        </>
      ) : (
        <Stack gap="6px">
          <Text size="md" fw={500}>
            {verifiedLogin ? i18n.trans('cloud_sync_verify_ok', [verifiedLogin]) : ''}
          </Text>
          {githubAppToken ? (
            <Alert color="yellow">
              {lang.cloud_sync_github_app_token_hint}
            </Alert>
          ) : null}
          <Stack gap="6px">
            <Text size="md" fw={500}>
              {lang.cloud_sync_repo}
            </Text>
            <TextInput
              value={repo}
              onChange={(e) => setRepo(e.target.value)}
              disabled={busy}
            />
            <Text size="sm" c="dimmed">
              {lang.cloud_sync_repo_help}
            </Text>
            {authError ? <Alert color="red">{authError}</Alert> : null}
            {createRepoNeeded ? (
              <Alert color="blue">
                <Stack gap="10px">
                  <Text size="md">{lang.sync_error_create_repo_not_allowed}</Text>
                  <Group gap="12px">
                    <Button
                      variant="light"
                      onClick={() =>
                        actions
                          .openUrl(
                            `https://github.com/new?name=${encodeURIComponent(
                              repo.trim(),
                            )}&description=${encodeURIComponent('SwitchHosts cloud backup')}`,
                          )
                          .catch((e) => console.error(e))
                      }
                    >
                      {lang.cloud_sync_create_repo_open_page}
                    </Button>
                    <Button onClick={runSetup} loading={busy}>
                      {lang.cloud_sync_retry_after_create}
                    </Button>
                  </Group>
                </Stack>
              </Alert>
            ) : null}
            <Group gap="12px">
              <Button variant="outline" onClick={() => setStep('auth')} disabled={busy}>
                {lang.btn_cancel}
              </Button>
              <Button onClick={runSetup} loading={busy} disabled={!repo.trim()}>
                {lang.import_next_step}
              </Button>
            </Group>
          </Stack>
        </Stack>
      )}
    </Stack>
  )
}

export default CloudSync
