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

// Per process, so "Not now" asks again after the next launch.
private var dismissedThisSession = false

/** A denied system prompt can't be shown again, so only Enable spends it. */
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
    val enable = {
        ChatPrefs.notifPrimed = true
        show = false
        launcher.launch(Manifest.permission.POST_NOTIFICATIONS)
    }
    val notNow = { dismissedThisSession = true; show = false }

    AppAlertDialog(
        onDismissRequest = notNow,
        title = { Text("Turn on notifications") },
        text = { Text("So you hear from your contacts when Promtuz is closed.") },
        confirmButton = { TextButton(onClick = enable) { Text("Enable") } },
        dismissButton = { TextButton(onClick = notNow) { Text("Not now") } },
    )
}
