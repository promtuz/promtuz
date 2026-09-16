package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

@Composable
fun AboutScreen(updates: UpdateVM = koinViewModel()) {
    val context = LocalContext.current
    val packageInfo = remember { context.packageManager.getPackageInfo(context.packageName, 0) }
    val channel = if (BuildConfig.DEBUG) "Debug" else "Release"
    var updateChannel by remember { mutableStateOf(updates.channel) }

    SimpleScreen({ Text("About Promtuz") }) { padding ->
        Column(
            Modifier.fillMaxSize().padding(padding).padding(horizontal = 20.dp, vertical = 18.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Text(
                "${packageInfo.versionName}  ·  $channel channel",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                style = MaterialTheme.typography.bodyLarge,
            )
            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(
                    "Update channel",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.labelLarge,
                )
                SingleChoiceSegmentedButtonRow {
                    val channels = listOf("release", "debug")
                    channels.forEachIndexed { index, option ->
                        SegmentedButton(
                            selected = updateChannel == option,
                            onClick = {
                                updateChannel = option
                                updates.switchChannel(option)
                            },
                            shape = SegmentedButtonDefaults.itemShape(index, channels.size),
                        ) { Text(option.replaceFirstChar { it.uppercase() }) }
                    }
                }
            }
            OutlinedButton(onClick = { updates.showSheet(check = true) }) { Text("Check for updates") }
        }
    }
}
