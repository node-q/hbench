"""A scripted browser for the Keycloak end-to-end test: it opens the authorization address, signs in with a
password, grants consent, and follows the redirect to hbench's loopback listener. Standard library only.

Usage: browser.py <authorization-url>   (user and password from E2E_USER / E2E_PASSWORD)
"""

import html.parser
import http.cookiejar
import os
import sys
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


class Form(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.action, self.fields, self.inside = None, {}, False

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "form" and self.action is None:
            self.action, self.inside = a.get("action"), True
        elif tag == "input" and self.inside and a.get("name") and a.get("type") != "submit":
            self.fields[a["name"]] = a.get("value") or ""
        elif tag in ("button", "input") and self.inside and a.get("name") == "accept":
            self.fields["accept"] = a.get("value") or "Yes"

    def handle_endtag(self, tag):
        if tag == "form":
            self.inside = False


class LoopbackIsSecure(http.cookiejar.DefaultCookiePolicy):
    """Browsers treat http://127.0.0.1 as a secure context and send `Secure` cookies there, as Keycloak expects."""

    def return_ok_secure(self, cookie, request):
        return True


jar = http.cookiejar.CookieJar(policy=LoopbackIsSecure())
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar), NoRedirect)


def fetch(url, data=None):
    req = urllib.request.Request(url, data=urllib.parse.urlencode(data).encode() if data is not None else None)
    try:
        r = opener.open(req, timeout=20)
        return r.status, r.headers.get("Location"), r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.headers.get("Location"), e.read().decode("utf-8", "replace")


def main(url):
    user, password = os.environ.get("E2E_USER", "pat"), os.environ.get("E2E_PASSWORD", "pat-password-e2e")
    status, location, body = fetch(url)
    for _ in range(20):
        if status in (301, 302, 303) and location:
            url = urllib.parse.urljoin(url, location)
            if url.startswith("http://127.0.0.1:") and "/callback" in url:
                status, _, body = fetch(url)  # hbench's loopback listener
                print(f"loopback: {status}: {body[:300]}")
                return 0 if status == 200 else 1
            status, location, body = fetch(url)
            continue
        form = Form()
        form.feed(body)
        if not form.action:
            print(f"no form: HTTP {status}\n{body[:2000]}")
            return 1
        fields = form.fields
        if "username" in fields or "password" in fields:
            fields.update(username=user, password=password)
        url = urllib.parse.urljoin(url, form.action.replace("&amp;", "&"))
        status, location, body = fetch(url, fields)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
