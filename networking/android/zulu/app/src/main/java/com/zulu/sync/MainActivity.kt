package com.zulu.sync

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity

/**
 * Pairing + entry point. You paste the host URL shown in the desktop Zulu window,
 * save it, then either open the web receiver (tap-to-copy) or just use the system
 * share sheet ("Send to Zulu") from any app.
 */
class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        val urlField = findViewById<EditText>(R.id.host_url)
        val save = findViewById<Button>(R.id.save)
        val open = findViewById<Button>(R.id.open_receiver)
        val status = findViewById<TextView>(R.id.status)

        urlField.setText(Host.get(this))
        refreshStatus(status)

        save.setOnClickListener {
            Host.set(this, urlField.text.toString())
            urlField.setText(Host.get(this))
            refreshStatus(status)
            Toast.makeText(this, R.string.saved, Toast.LENGTH_SHORT).show()
        }

        open.setOnClickListener {
            val host = Host.get(this)
            if (host.isEmpty()) {
                Toast.makeText(this, R.string.set_host_first, Toast.LENGTH_SHORT).show()
            } else {
                startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(host)))
            }
        }
    }

    private fun refreshStatus(status: TextView) {
        val host = Host.get(this)
        status.text = if (host.isEmpty()) getString(R.string.not_paired) else getString(R.string.paired_with, host)
    }
}
