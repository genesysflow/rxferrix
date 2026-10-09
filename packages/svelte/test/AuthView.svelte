<script lang="ts">
  import { useAuth, useCurrentUser } from "../src/lib/index.js";

  const auth = useAuth();
  const user = useCurrentUser();
</script>

<p>
  {auth.isLoading ? "loading" : auth.isAuthenticated ? "in" : "out"}
  /
  {user.data === undefined ? "?" : user.data === null ? "none" : user.data.email}
</p>
<button onclick={() => auth.signIn({ email: "a@b.co", password: "pw" })}>in</button>
<button onclick={() => auth.signOut()}>out</button>
