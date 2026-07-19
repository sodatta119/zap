package com.zulu.sync

import android.content.Context
import java.net.HttpURLConnection
import java.net.URL

/**
 * Zulu on Android is an *assisted client*: it doesn't host or read the clipboard
 * in the background (the OS forbids that), it just pushes what you explicitly
 * share to the paired desktop host, and opens the host's web page for tap-to-copy
 * receiving. So all it needs is the host URL and a plain HTTP POST - no native
 * code, no service.
 */
object Host {
    private const val PREFS = "zulu"
    private const val KEY_URL = "host_url"

    fun get(ctx: Context): String =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(KEY_URL, "") ?: ""

    fun set(ctx: Context, url: String) {
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putString(KEY_URL, normalize(url)).apply()
    }

    /** Accept "192.168.1.9:8080", "host:8080/", or a full URL; store a clean base. */
    fun normalize(raw: String): String {
        var s = raw.trim()
        if (s.isEmpty()) return s
        if (!s.startsWith("http://") && !s.startsWith("https://")) s = "http://$s"
        return s.trimEnd('/')
    }
}

object Clip {
    /** POST [text] to `<host>/clip`. Blocking - call off the main thread. */
    fun send(host: String, text: String): Boolean {
        return try {
            val conn = URL("$host/clip").openConnection() as HttpURLConnection
            conn.requestMethod = "POST"
            conn.doOutput = true
            conn.connectTimeout = 4000
            conn.readTimeout = 4000
            conn.setRequestProperty("Content-Type", "text/plain; charset=utf-8")
            conn.outputStream.use { it.write(text.toByteArray(Charsets.UTF_8)) }
            val ok = conn.responseCode in 200..299
            conn.disconnect()
            ok
        } catch (e: Exception) {
            false
        }
    }
}
