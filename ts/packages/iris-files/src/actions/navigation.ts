/**
 * Navigation actions - file selection and directory navigation
 */
import { navigate } from '../utils/navigate';
import { parseRoute } from '../utils/route';
import { LinkType, type Hash } from 'hashtree';
import { updateRoute, buildRouteUrl, getCurrentPathFromUrl } from './route';

// Clear file selection (navigate to current directory without file)
export function clearFileSelection() {
  updateRoute();
}

// Navigate to directory
export function navigateTo(_hash: Hash, name?: string) {
  if (name) {
    const route = parseRoute();
    const currentPath = getCurrentPathFromUrl();
    const newPath = [...currentPath, name];
    const url = buildRouteUrl(route.npub, route.treeName, newPath, undefined, route.params.get('k'));
    navigate(url);
  }
}

// Go back in path
export function goBack() {
  const currentPath = getCurrentPathFromUrl();
  if (currentPath.length === 0) return;

  const newPath = currentPath.slice(0, -1);
  const route = parseRoute();
  const url = buildRouteUrl(route.npub, route.treeName, newPath, undefined, route.params.get('k'));
  navigate(url);
}

// Select file for viewing
export function selectFile(entry: { name: string; type: LinkType } | null) {
  if (!entry || entry.type === LinkType.Dir) return;
  updateRoute(entry.name);
}
