package com.zulu.sync

import android.content.Intent
import android.os.Bundle
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import kotlin.concurrent.thread

/**
 * The share-sheet handler. When you Share text or a link and pick "Zulu", this
 * pushes it to the paired host's clipboard and finishes - no UI of its own. This
 * is the Android "send" path (reading the clipboard in the background is blocked
 * by the OS, so an explicit share is how you send from a phone).
 */
class ShareActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val text = if (intent?.action == Intent.ACTION_SEND && intent.type == "text/plain") {
            intent.getStringExtra(Intent.EXTRA_TEXT)
        } else {
            null
        }

        if (text.isNullOrEmpty()) {
            toastAndFinish(getString(R.string.nothing_to_send))
            return
        }

        val host = Host.get(this)
        if (host.isEmpty()) {
            toastAndFinish(getString(R.string.set_host_first))
            startActivity(Intent(this, MainActivity::class.java))
            return
        }

        thread {
            val ok = Clip.send(host, text)
            runOnUiThread {
                toastAndFinish(if (ok) getString(R.string.sent) else getString(R.string.send_failed))
            }
        }
    }

    private fun toastAndFinish(msg: String) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
        finish()
    }
}
