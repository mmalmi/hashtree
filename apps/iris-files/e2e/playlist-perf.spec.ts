/**
 * Playlist performance test with CPU profiling
 *
 * Uses Chrome DevTools Protocol to capture real CPU profiles during scrolling.
 *
 * Run: npx playwright test playlist-perf.spec.ts --project=chromium --reporter=list
 */
import { test } from './fixtures';
import * as fs from 'fs';

const PLAYLIST_URL = '#/npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk/videos%2FAngel%20Sword/60m84vRAsZw';

test.describe('Playlist Performance', () => {
  test('CPU profile playlist scrolling', async ({ page }) => {
    test.setTimeout(180000);

    // Get CDP session for profiling
    const client = await page.context().newCDPSession(page);

    const logs: string[] = [];
    page.on('console', msg => {
      const text = msg.text();
      logs.push(`[${msg.type()}] ${text}`);
      // Print function call counts in real-time
      if (text.includes('calls=') || text.includes('render')) {
        console.log(text);
      }
    });

    console.log('=== Loading Playlist ===');
    await page.goto(`http://localhost:5173/video.html${PLAYLIST_URL}`);

    // Wait for video
    try {
      await page.waitForSelector('video', { timeout: 45000 });
      console.log('Video loaded');
    } catch {
      console.log('Video not found');
    }

    // Wait for page to stabilize and playlist to load
    await page.waitForTimeout(10000);

    // Check what's on the page
    const bodyText = await page.locator('body').innerText();
    console.log('Page content preview:', bodyText.slice(0, 300));

    // Check for playlist sidebar
    const sidebarButtons = await page.locator('.overflow-auto button').count();
    console.log(`Sidebar buttons: ${sidebarButtons}`);

    // Inject function call counter
    await page.evaluate(() => {
      const win = window as any;
      win.__fnCalls = {};
      win.__countCall = (name: string) => {
        win.__fnCalls[name] = (win.__fnCalls[name] || 0) + 1;
      };
      win.__printCalls = () => {
        const sorted = Object.entries(win.__fnCalls)
          .sort((a: any, b: any) => b[1] - a[1])
          .slice(0, 20);
        console.log('=== Function Call Counts ===');
        sorted.forEach(([name, count]) => {
          console.log(`${name}: ${count} calls`);
        });
      };
    });

    console.log('\n=== Starting CPU Profile ===');

    // Enable profiling
    await client.send('Profiler.enable');
    await client.send('Profiler.start');

    // Perform scroll operations
    const sidebar = page.locator('.overflow-auto').first();
    const hasSidebar = await sidebar.count() > 0;

    if (hasSidebar) {
      console.log('Scrolling sidebar...');
      for (let i = 0; i < 20; i++) {
        await sidebar.evaluate(el => { el.scrollTop += 100; });
        await page.waitForTimeout(30);
        await sidebar.evaluate(el => { el.scrollTop -= 50; });
        await page.waitForTimeout(30);
      }
    } else {
      console.log('No sidebar, scrolling page...');
      for (let i = 0; i < 20; i++) {
        await page.mouse.wheel(0, 200);
        await page.waitForTimeout(30);
        await page.mouse.wheel(0, -100);
        await page.waitForTimeout(30);
      }
    }

    // Stop profiling and get results
    const { profile } = await client.send('Profiler.stop');
    await client.send('Profiler.disable');

    // Save profile for Chrome DevTools
    const profilePath = '/tmp/playlist-scroll-profile.cpuprofile';
    fs.writeFileSync(profilePath, JSON.stringify(profile));
    console.log(`\nCPU profile saved to: ${profilePath}`);
    console.log('Open in Chrome DevTools: chrome://inspect -> Open dedicated DevTools -> Performance -> Load profile');

    // Analyze the profile - find hottest functions
    console.log('\n=== Top Functions by Self Time ===');
    const nodes = profile.nodes || [];
    const samples = profile.samples || [];
    const timeDeltas = profile.timeDeltas || [];

    // Count samples per node
    const sampleCounts: Record<number, number> = {};
    samples.forEach((nodeId: number) => {
      sampleCounts[nodeId] = (sampleCounts[nodeId] || 0) + 1;
    });

    // Calculate time per node (rough estimate based on sample count)
    const totalSamples = samples.length;
    const totalTime = timeDeltas.reduce((a: number, b: number) => a + b, 0);
    const timePerSample = totalTime / totalSamples;

    interface NodeInfo {
      id: number;
      name: string;
      url: string;
      samples: number;
      time: number;
    }

    const nodeInfos: NodeInfo[] = nodes.map((node: any) => {
      const callFrame = node.callFrame || {};
      return {
        id: node.id,
        name: callFrame.functionName || '(anonymous)',
        url: callFrame.url || '',
        samples: sampleCounts[node.id] || 0,
        time: (sampleCounts[node.id] || 0) * timePerSample / 1000, // ms
      };
    });

    // Sort by time and show top functions
    const hotFunctions = nodeInfos
      .filter(n => n.samples > 0 && n.name !== '(idle)' && n.name !== '(program)')
      .sort((a, b) => b.time - a.time)
      .slice(0, 30);

    hotFunctions.forEach((fn, i) => {
      const urlShort = fn.url.split('/').slice(-2).join('/');
      console.log(`${i + 1}. ${fn.name} - ${fn.time.toFixed(1)}ms (${fn.samples} samples) [${urlShort}]`);
    });

    // Group by file
    console.log('\n=== Time by File ===');
    const timeByFile: Record<string, number> = {};
    nodeInfos.forEach(n => {
      if (n.url && n.samples > 0) {
        const file = n.url.split('/').slice(-1)[0] || n.url;
        timeByFile[file] = (timeByFile[file] || 0) + n.time;
      }
    });

    Object.entries(timeByFile)
      .sort((a, b) => b[1] - a[1])
      .slice(0, 15)
      .forEach(([file, time]) => {
        console.log(`${file}: ${time.toFixed(1)}ms`);
      });

    // Print function call counts if any were recorded
    await page.evaluate(() => {
      (window as any).__printCalls?.();
    });

    // Print relevant logs
    console.log('\n=== Relevant Logs ===');
    logs
      .filter(l => l.includes('render') || l.includes('effect') || l.includes('Error'))
      .slice(-20)
      .forEach(l => console.log(l));

    console.log('\n=== Test Complete ===');
  });
});
