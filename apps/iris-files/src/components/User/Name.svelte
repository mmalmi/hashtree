<script lang="ts">
  import { createProfileStore, getProfileName } from '../../stores/profile';
  import { animalName } from '../../utils/animalName';

  interface Props {
    pubkey: string;
    class?: string;
  }

  let { pubkey, class: className = '' }: Props = $props();

  let profileStore = $derived(pubkey ? createProfileStore(pubkey) : null);
  let profile = $derived(profileStore ? $profileStore : null);
  let profileName = $derived(getProfileName(profile, pubkey));
  let animal = $derived(pubkey ? animalName(pubkey) : '');
</script>

{#if profileName}
  <span class="truncate {className}">{profileName}</span>
{:else}
  <!-- Animal name fallback (styled differently) -->
  <span class="truncate italic opacity-70 {className}">
    {animal}
  </span>
{/if}
