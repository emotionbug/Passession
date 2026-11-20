// Runs in OMP's extension loader. The RPC state and logout list do not expose explicit affinity.
const command = "__PASEO_ACCOUNT_COMMAND__";

export default function paseoOmp(pi) {
  const accountCommand = {
    description: "Passession internal OAuth account control",
    async handler(args, ctx) {
      const { requestId, provider, sessionId, credentialId } = JSON.parse(
        Buffer.from(args, "base64url").toString("utf8"),
      );
      if (typeof requestId !== "string") return;
      const reply = (result) =>
        ctx.ui.notify("PASEO_COMMAND_RESULT " + JSON.stringify({ requestId, result }), "info");
      const fail = (error) =>
        ctx.ui.notify("PASEO_COMMAND_RESULT " + JSON.stringify({ requestId, ok: false, error }), "info");
      const storage = ctx.modelRegistry?.authStorage;
      const sessions = storage?.sessions;
      if (
        typeof provider !== "string" || typeof sessionId !== "string" ||
        provider !== ctx.model?.provider || typeof sessions?.get !== "function" ||
        typeof sessions.pin !== "function" || typeof sessions.release !== "function" ||
        typeof storage?.oauth?.accounts !== "function" ||
        typeof storage?.credentials?.reload !== "function"
      ) {
        if (credentialId === undefined) return reply(null);
        return fail("This OMP runtime does not expose native OAuth account selection.");
      }
      try {
        await storage.credentials.reload();
        if (provider !== ctx.model?.provider) return fail("The current OMP provider changed.");
        const accounts = storage.oauth.accounts(provider, sessionId);
        if (credentialId !== undefined) {
          if (!ctx.isIdle()) return fail("Wait for the current OMP turn to end before changing accounts.");
          if (credentialId === null) {
            if (sessions.get(provider, sessionId)?.explicit === true) sessions.release(provider, sessionId);
          } else if (!accounts.some((account) => account.credentialId === credentialId) ||
                     !sessions.pin(provider, sessionId, credentialId)) {
            return fail("The selected OAuth account is no longer available to pin.");
          }
        }
        const pin = sessions.get(provider, sessionId);
        reply({
          explicit: pin?.type === "oauth" && pin.explicit === true ? pin.credentialId : null,
          accounts: accounts.map((account) => {
            const label = account.email || account.accountId || account.projectId ||
              account.enterpriseUrl || `OAuth credential #${account.credentialId}`;
            const org = account.orgName || account.orgId;
            return {
              id: account.credentialId,
              label: org && org !== label ? `${label} (${org})` : label,
            };
          }),
        });
      } catch {
        fail("OMP could not read or change this session's OAuth account.");
      }
    },
  };
  // The RPC runner binds the real UI (or a no-op context for headless hosts)
  // before session_start. Do not advertise a command that cannot reply.
  pi.on("session_start", (_event, ctx) => {
    if (ctx.hasUI) pi.registerCommand(command, accountCommand);
  });
}
