<script lang="ts">
  // Test-only harness: owns the bound header string so a test can read back
  // exactly what the field publishes to its parent. `bind:` props are not
  // readable off a mounted Svelte 5 component, and what the parent ends up
  // sending is the whole point of this control.
  //
  // The value is reported through a callback rather than rendered, so the
  // harness adds no nodes that queries in the test could pick up by accident.
  import { untrack } from 'svelte';
  import RecipientField from './RecipientField.svelte';
  import type { Suggester } from '$lib/recipient-suggestions';

  let {
    value: incoming = '',
    id = 'field-to',
    label = 'To',
    accountId = 'acc1',
    disabled = false,
    exclude = [],
    placeholder = '',
    invalid = false,
    suggester,
    onvalue
  }: {
    value?: string;
    id?: string;
    label?: string;
    accountId?: string;
    disabled?: boolean;
    exclude?: string[];
    placeholder?: string;
    invalid?: boolean;
    suggester?: Suggester;
    onvalue?: (value: string) => void;
  } = $props();

  let value = $state(untrack(() => incoming));

  // Mirrors a parent replacing the value (route change, draft reload), which is
  // what `rerender({ value })` exercises.
  $effect(() => {
    value = incoming;
  });

  $effect(() => {
    onvalue?.(value);
  });
</script>

<RecipientField
  bind:value
  {id}
  {label}
  {accountId}
  {disabled}
  {exclude}
  {placeholder}
  {invalid}
  {...suggester ? { suggester } : {}}
/>
