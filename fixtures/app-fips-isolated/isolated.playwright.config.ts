import original from './playwright.config'

if (process.env.PLAYWRIGHT_PORT !== '5721') throw new Error('Unexpected Chat fixture origin')
export default {
  ...original,
  use: { ...original.use, trace: 'retain-on-failure' },
  // Keep every original project, device, grep, channel and testMatch.
  projects: original.projects?.map(project => ({
    ...project,
    use: {
      ...project.use,
      ...(project.name === 'chromium' ? {
        launchOptions: {
          ...project.use?.launchOptions,
          args: [
            ...(project.use?.launchOptions?.args ?? []),
            '--disable-features=WebRtcHideLocalIpsWithMdns',
            '--allow-loopback-in-peer-connection',
          ],
        },
      } : {}),
    },
  })),
  // The runner owns and verifies the exact dist preview before Playwright.
  // A missing preview must fail instead of rebuilding the release candidate.
  webServer: undefined,
}
