package io.github.ilyalissoboi.emusync

import android.app.Activity
import android.os.Bundle
import android.util.Log

/**
 * Receives Dropbox's redirect at the end of a sign-in ([DropboxSignIn.redirectUri]), keeps it for
 * [DropboxSignIn.complete], and brings the activity that began the sign-in back over the Custom
 * Tab. Declared by the AAR's manifest; shows nothing.
 */
class DropboxRedirectActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val data = intent?.data
        if (data != null) {
            DropboxSignIn.onRedirect(this, data)?.let { back ->
                try {
                    startActivity(back)
                } catch (e: RuntimeException) {
                    Log.w("emusync", "can't return to the Dropbox sign-in's activity", e)
                }
            }
        }
        finish()
    }
}
