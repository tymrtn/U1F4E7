<script lang="ts">
  // Reader route — thin shell that mounts the ReaderPane component.
  // All reader logic lives in ReaderPane; the route only provides the scaffold.
  import ReaderPane from '$lib/components/ReaderPane.svelte';
</script>

<ReaderPane />
