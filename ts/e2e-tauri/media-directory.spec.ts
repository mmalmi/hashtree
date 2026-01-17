/**
 * Tauri E2E test for loading media directory
 *
 * Verifies that npub1g53.../media shows files, not "empty directory"
 * This was a regression where Tauri backend couldn't fetch blobs from Blossom
 */
import { browser, $ } from '@wdio/globals';

const TEST_NPUB = 'npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk';
const TEST_TREE = 'media';

describe('Media directory loading', () => {
  it('should load media tree and show files', async () => {
    const url = `tauri://localhost/files.html#/${TEST_NPUB}/${TEST_TREE}`;
    await browser.url(url);

    const knownFiles = ['ekiss.jpeg', 'bitcoin.pdf', 'glacier montana.jpg'];
    const getDirectoryState = async () => {
      return browser.execute((expectedFiles) => {
        const items = document.querySelectorAll(
          '[data-testid="file-item"], [data-testid="folder-item"], .file-item, .folder-item'
        );
        const bodyText = (document.body?.textContent || '').toLowerCase();
        const hasEmptyMessage =
          bodyText.includes('empty directory') || bodyText.includes('no files');
        const hasKnownFile = expectedFiles.some((name) =>
          bodyText.includes(name.toLowerCase())
        );

        return {
          itemCount: items.length,
          hasEmptyMessage,
          hasKnownFile,
        };
      }, knownFiles);
    };

    await browser.waitUntil(
      async () => {
        const state = await getDirectoryState();
        if (state.hasEmptyMessage) {
          throw new Error('Directory shows as empty - Blossom fallback may not be working');
        }
        return state.itemCount > 0 || state.hasKnownFile;
      },
      {
        timeout: 30000,
        interval: 500,
        timeoutMsg: 'Media directory did not render files',
      }
    );

    const finalState = await getDirectoryState();
    expect(finalState.itemCount > 0 || finalState.hasKnownFile).toBe(true);
  });

  it('should not show empty directory message', async () => {
    // The URL should still be on the media tree from previous test
    const pageText = await browser.execute(() => {
      return document.body?.textContent?.toLowerCase() || '';
    });

    // Should NOT contain empty directory indicators
    expect(pageText).not.toContain('empty directory');
    expect(pageText).not.toContain('no files found');
  });
});
