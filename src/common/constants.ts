/**
 * constants
 * @author: oldj
 * @homepage: https://oldj.net
 */

export const serverUrl = 'https://switchhosts.vercel.app'
export const homepageUrl = 'https://switchhosts.app'
export const downloadUrl = `${serverUrl}/download/`
export const sourceUrl = 'https://github.com/oldj/SwitchHosts'
export const feedbackUrl = 'https://github.com/oldj/SwitchHosts/issues'
export const defaultHttpApiPort = 50761

/// Built-in client id of a GitHub OAuth App with Device Flow enabled.
/// Device flow needs no client secret, so emitting the public client id in
/// the bundle is by design and safe. When the maintainers register an app,
/// set this value; the settings page then hides the client id field and
/// users sign in with zero setup. Left empty for unofficial builds; the UI
/// falls back to letting users paste their own client id.
export const githubDeviceClientId = ''
