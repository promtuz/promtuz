package com.promtuz.chat.ui.components

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import com.promtuz.chat.data.ChatPrefs

// Suppresses the primer for the rest of this process after "Not now" — resets on next launch so the
// ask returns at a later high-intent moment, without persisting a permanent opt-out.
private var dismissedThisSession = false

/**
 * Contextual, one-shot priming for POST_NOTIFICATIONS. The system prompt is a
 * one-shot on 13+ (denials can't be re-summoned), so we don't burn it head-on at
 * cold launch — this fires at a high-intent moment (entering a chat). A tiny
 * in-app step first; only tapping Enable fires the real system dialog, so a
 * hesitant "Not now" keeps it in reserve. Receiving/draining never needs this —
 * it only gates whether a banner shows, so denial degrades to silent sync.
 */
@Composable
fun NotificationPrimer() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
    val context = LocalContext.current
    var show by remember {
        mutableStateOf(
            !ChatPrefs.notifPrimed && !dismissedThisSession &&
                ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
                PackageManager.PERMISSION_GRANTED,
        )
    }
    if (!show) return

    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { }
    // Enable spends the one-shot system prompt → done for good. "Not now" only suppresses this session.
    val enable = {
        ChatPrefs.notifPrimed = true
        show = false
        launcher.launch(Manifest.permission.POST_NOTIFICATIONS)
    }
    val notNow = { dismissedThisSession = true; show = false }

    AlertDialog(
        onDismissRequest = notNow,
        title = { Text("Turn on notifications") },
        text = { Text("So you hear from your contacts when Promtuz is closed.") },
        confirmButton = { TextButton(onClick = enable) { Text("Enable") } },
        dismissButton = { TextButton(onClick = notNow) { Text("Not now") } },
    )
}
