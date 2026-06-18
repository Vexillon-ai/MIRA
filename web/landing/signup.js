// MIRA landing-page signup form. Posts to MIRA's own
// /api/waitlist/signup endpoint — same-origin when the landing page
// is served from MIRA, or cross-origin (CORS) when deployed as a
// separate static site. Override WAITLIST_ENDPOINT below for the
// cross-origin case.

(function () {
  // Same-origin by default. When hosting the landing on a separate
  // domain (mira.example.com landing → api.example.com MIRA), set
  // window.WAITLIST_ENDPOINT before this script loads:
  //   <script>window.WAITLIST_ENDPOINT = 'https://your-mira-host/api/waitlist/signup';</script>
  //   <script src="signup.js"></script>
  const endpoint = window.WAITLIST_ENDPOINT || '/api/waitlist/signup'

  const form   = document.getElementById('waitlist-form')
  const input  = document.getElementById('email')
  const status = document.getElementById('form-status')
  const button = form?.querySelector('button[type="submit"]')

  if (!form || !input || !status) return

  function setStatus(text, kind) {
    status.textContent = text
    status.classList.remove('ok', 'error')
    if (kind) status.classList.add(kind)
  }

  form.addEventListener('submit', async (e) => {
    e.preventDefault()
    const email = input.value.trim()
    if (!email || !email.includes('@')) {
      setStatus('That doesn\'t look like an email address.', 'error')
      return
    }

    button.disabled = true
    setStatus('Adding you…')

    try {
      const resp = await fetch(endpoint, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ email, source: 'landing' }),
      })
      if (!resp.ok) {
        let msg = `HTTP ${resp.status}`
        try {
          const body = await resp.json()
          if (body?.error) msg = body.error
        } catch { /* non-JSON body, fall through */ }
        throw new Error(msg)
      }
      const data = await resp.json().catch(() => ({}))
      input.value = ''
      const pos = data?.position
      setStatus(
        pos ? `You're #${pos} on the list. I'll be in touch.` : `You're on the list. I'll be in touch.`,
        'ok',
      )
    } catch (err) {
      setStatus(`Couldn't add you: ${err.message}`, 'error')
    } finally {
      button.disabled = false
    }
  })
})()
