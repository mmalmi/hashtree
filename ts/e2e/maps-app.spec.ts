import { test, expect } from './fixtures';
import { setupPageErrorHandler, waitForAppReady, disableOthersPool } from './test-utils';

/**
 * Tests for maps.iris.to (Iris Maps app)
 * Tests map loading, tile rendering, and basic navigation
 */
test.describe('Iris Maps App', () => {
  test.beforeEach(async ({ page }) => {
    setupPageErrorHandler(page);
  });

  test('shows Iris Maps header', async ({ page }) => {
    await page.goto('/maps.html#/');
    await expect(page.locator('text=iris maps')).toBeVisible({ timeout: 30000 });
  });

  test('shows map container with Leaflet', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Leaflet creates a container with class 'leaflet-container'
    await expect(page.locator('.leaflet-container')).toBeVisible({ timeout: 30000 });
  });

  test('map has zoom controls', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Leaflet zoom controls
    await expect(page.locator('.leaflet-control-zoom')).toBeVisible({ timeout: 30000 });
  });

  test('shows Add Place button', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Wait for map tiles to load
    await page.waitForSelector('.leaflet-tile-loaded', { timeout: 30000 });

    // Should show Add Place button in map controls (use force check since icon might have zero dimensions)
    const addPlaceBtn = page.locator('button[title="Add Place"]');
    await expect(addPlaceBtn).toBeAttached({ timeout: 30000 });
  });

  test('shows audience filter control', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Should show audience filter dropdown/control
    await expect(page.locator('[data-testid="audience-filter"]')).toBeVisible({ timeout: 30000 });
  });

  test('can open Add Place modal', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Wait for the map tiles to load
    await page.waitForSelector('.leaflet-tile-loaded', { timeout: 30000 });

    // Click Add Place button
    const addPlaceBtn = page.locator('button[title="Add Place"]');
    await addPlaceBtn.scrollIntoViewIfNeeded();
    await addPlaceBtn.click({ force: true, timeout: 10000 });

    // Modal should appear with form fields
    await expect(page.getByRole('heading', { name: 'Add Place' })).toBeVisible({ timeout: 10000 });
    await expect(page.locator('input[placeholder="Place name..."]')).toBeVisible();
    await expect(page.locator('select[name="category"]')).toBeVisible();
  });

  test('can add a place and see marker on map', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Wait for the map to be ready
    await expect(page.locator('.leaflet-container')).toBeVisible({ timeout: 30000 });

    // Click Add Place button
    const addPlaceBtn = page.locator('button[title="Add Place"]');
    await addPlaceBtn.scrollIntoViewIfNeeded();
    await addPlaceBtn.click({ force: true, timeout: 10000 });

    // Wait for modal to appear
    await expect(page.getByRole('heading', { name: 'Add Place' })).toBeVisible({ timeout: 10000 });

    // Fill in place details
    const placeName = `Test Place ${Date.now()}`;
    await page.locator('input[placeholder="Place name..."]').fill(placeName);
    await page.locator('select[name="category"]').selectOption('restaurant');

    // Click on map to set location (right side of screen, away from modal)
    await page.locator('.leaflet-container').click({ position: { x: 600, y: 300 }, force: true });

    // Wait for location to be set
    await expect(page.locator('text=Location:')).toBeVisible({ timeout: 5000 });

    // Save the place
    await page.getByRole('button', { name: 'Save' }).click();

    // Modal should close
    await expect(page.getByRole('heading', { name: 'Add Place' })).not.toBeVisible({ timeout: 10000 });

    // Marker should appear on map
    await expect(page.locator('.leaflet-marker-icon')).toBeVisible({ timeout: 10000 });
  });

  test('audience filter changes visible places', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    // Select "Own" filter
    await page.locator('[data-testid="audience-filter"]').selectOption('own');

    // Filter should be set
    await expect(page.locator('[data-testid="audience-filter"]')).toHaveValue('own');
  });

  test('shows search input', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Should show search input
    await expect(page.locator('[data-testid="search-input"]')).toBeVisible({ timeout: 30000 });
  });

  test('search shows results dropdown', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Type a search query
    const searchInput = page.locator('[data-testid="search-input"]');
    await searchInput.fill('London');

    // Wait for search results to appear
    await expect(page.locator('[data-testid="search-results"]')).toBeVisible({ timeout: 10000 });

    // Should have at least one result
    await expect(page.locator('[data-testid="search-results"] button').first()).toBeVisible();
  });

  test('search suggests Stockholm for stockh', async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem('iris-maps-view', JSON.stringify({ center: [59.325, 18.07], zoom: 10 }));
    });
    await page.goto('/maps.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    const searchInput = page.locator('[data-testid="search-input"]');
    await searchInput.fill('stockh');

    await expect(page.locator('[data-testid="search-results"]')).toBeVisible({ timeout: 10000 });
    await expect(
      page.locator('[data-testid="search-results"] button', { hasText: 'Stockholm' }).first()
    ).toBeVisible({ timeout: 10000 });
  });

  test('short query completion suggests Rovaniemi', async ({ page }) => {
    test.slow();
    await page.addInitScript(() => {
      localStorage.setItem('iris-maps-view', JSON.stringify({ center: [59.325, 18.07], zoom: 10 }));
    });
    await page.goto('/maps.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    const searchInput = page.locator('[data-testid="search-input"]');
    await searchInput.fill('rovani');

    const results = page.locator('[data-testid="search-results"]');
    await expect(results).toBeVisible({ timeout: 15000 });
    await expect(
      page.locator('[data-testid="search-results"] button', { hasText: 'Rovaniemi' }).first()
    ).toBeVisible({ timeout: 15000 });
  });

  test('global queries surface major places from far away', async ({ page }) => {
    test.slow();
    await page.addInitScript(() => {
      localStorage.setItem('iris-maps-view', JSON.stringify({ center: [66.503, 25.729], zoom: 8 }));
    });
    await page.goto('/maps.html#/');
    await waitForAppReady(page);
    await disableOthersPool(page);

    const searchInput = page.locator('[data-testid="search-input"]');
    await searchInput.fill('caracas');

    const results = page.locator('[data-testid="search-results"]');
    await expect(results).toBeVisible({ timeout: 20000 });
    await expect(
      page.locator('[data-testid="search-results"] button', { hasText: 'Caracas' }).first()
    ).toBeVisible({ timeout: 20000 });

    await searchInput.fill('laos');
    await expect(results).toBeVisible({ timeout: 20000 });
    await expect(
      page.locator('[data-testid="search-results"] button', { hasText: 'Laos' }).first()
    ).toBeVisible({ timeout: 20000 });
  });

  test('map position persists after reload', async ({ page }) => {
    await page.goto('/maps.html#/');
    await waitForAppReady(page);

    // Wait for map to be ready
    await expect(page.locator('.leaflet-container')).toBeVisible({ timeout: 30000 });

    // Zoom in using the zoom control
    await page.locator('.leaflet-control-zoom-in').click();
    await page.waitForFunction(() => {
      const stored = localStorage.getItem('iris-maps-view');
      if (!stored) return false;
      try {
        const zoom = JSON.parse(stored).zoom;
        return typeof zoom === 'number' && zoom > 2;
      } catch {
        return false;
      }
    });

    // Get current zoom level from localStorage
    const savedView = await page.evaluate(() => {
      return localStorage.getItem('iris-maps-view');
    });
    expect(savedView).toBeTruthy();
    const parsedView = JSON.parse(savedView!);
    expect(parsedView.zoom).toBeGreaterThan(2); // Should be more than default world zoom

    // Reload the page
    await page.reload();
    await waitForAppReady(page);
    await expect(page.locator('.leaflet-container')).toBeVisible({ timeout: 30000 });

    // Check that the zoom level was restored
    const restoredView = await page.evaluate(() => {
      return localStorage.getItem('iris-maps-view');
    });
    const parsedRestored = JSON.parse(restoredView!);
    expect(parsedRestored.zoom).toBe(parsedView.zoom);
  });
});
