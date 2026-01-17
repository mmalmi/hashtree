/**
 * Visibility utilities - simplified version for hashtree-web
 * In the full iris-client, this uses social graph to filter content
 */

export const shouldHideEvent = (
  _event: {
    pubkey: string
    tags: Array<Array<string>>
  },
  _threshold = 1,
  _allowUnknown = false
): boolean => {
  // Don't hide any events in hashtree-web
  return false
}

export const shouldHideUser = (
  _pubKey: string,
  _threshold = 1,
  _allowUnknown = false
): boolean => {
  // Don't hide any users in hashtree-web
  return false
}
