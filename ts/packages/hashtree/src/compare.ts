const utf8 = new TextEncoder();

export function compareNames(left: string, right: string): number {
  const leftBytes = utf8.encode(left);
  const rightBytes = utf8.encode(right);
  const sharedLength = Math.min(leftBytes.length, rightBytes.length);

  for (let i = 0; i < sharedLength; i++) {
    const diff = leftBytes[i] - rightBytes[i];
    if (diff !== 0) return diff;
  }

  if (leftBytes.length < rightBytes.length) return -1;
  if (leftBytes.length > rightBytes.length) return 1;
  return 0;
}
