package com.promtuz.chat.ui.screens

import android.content.Intent
import androidx.activity.compose.BackHandler
import androidx.camera.core.ExperimentalGetImage
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.wrapContentSize
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ShareIdentityVM
import com.promtuz.chat.ui.activities.QrScanner
import com.promtuz.chat.ui.components.BackTopBar
import com.promtuz.chat.ui.components.IdentityHexGrid
import com.promtuz.chat.ui.components.IdentityQrCode
import com.promtuz.core.events.IdentityEvent

@Composable
fun ShareIdentityScreen(
    viewModel: ShareIdentityVM
) {
    val colors = MaterialTheme.colorScheme
    val identityRequest by viewModel.identityRequest.collectAsState()

    Scaffold(
        topBar = { BackTopBar("Share Identity") }
    ) { innerPadding ->
        Box(
            Modifier
                .fillMaxSize()
                .padding(innerPadding)
                .background(colors.background),
        ) {
            Column(
                Modifier.fillMaxSize(),
                verticalArrangement = Arrangement.spacedBy(48.dp, Alignment.CenterVertically)
            ) {
                Box(
                    Modifier
                        .wrapContentSize()
                        .align(Alignment.CenterHorizontally)
                ) {
                    IdentityQrCode(viewModel.qrData.collectAsState())
                }

                Column(
                    Modifier.fillMaxWidth(),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(12.dp)
                ) {
                    ScanQRButton()
                }
            }

            // Show identity request dialog as overlay
            identityRequest?.let { request ->
                IdentityRequestDialog(
                    request = request,
                    onAccept = { viewModel.acceptRequest() },
                    onReject = { viewModel.rejectRequest() }
                )
            }
        }
    }
}


@Composable
private fun IdentityRequestDialog(
    request: IdentityEvent.AddMe,
    onAccept: () -> Unit,
    onReject: () -> Unit
) {
    // Back press rejects the request
    BackHandler { onReject() }

    Dialog(
        onDismissRequest = onReject,
        properties = DialogProperties(
            dismissOnBackPress = true,
            dismissOnClickOutside = false
        )
    ) {
        Card(
            shape = RoundedCornerShape(28.dp),
            elevation = CardDefaults.cardElevation(8.dp),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.surfaceContainerHigh
            ),
            modifier = Modifier.fillMaxWidth()
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(20.dp)
            ) {
                // Header
                Text(
                    "Identity Request",
                    style = MaterialTheme.typography.titleLargeEmphasized,
                    color = MaterialTheme.colorScheme.onSurface
                )

                // Identity visual (hex grid of their public key)
                Card(
                    shape = RoundedCornerShape(16.dp),
                    colors = CardDefaults.cardColors(
                        containerColor = MaterialTheme.colorScheme.surfaceContainerLow
                    ),
                    modifier = Modifier.size(200.dp)
                ) {
                    Box(
                        modifier = Modifier
                            .fillMaxSize()
                            .padding(12.dp)
                    ) {
                        IdentityHexGrid(
                            key = request.ipk,
                            textStyle = MaterialTheme.typography.titleMediumEmphasized
                        )
                    }
                }

                // Name
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(4.dp)
                ) {
                    Text(
                        request.name,
                        style = MaterialTheme.typography.headlineSmall.copy(
                            fontWeight = FontWeight.SemiBold
                        ),
                        color = MaterialTheme.colorScheme.onSurface,
                        textAlign = TextAlign.Center
                    )

                    Text(
                        "wants to add you as a contact",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = TextAlign.Center
                    )
                }

                // Action buttons
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(12.dp)
                ) {
                    OutlinedButton(
                        onClick = onReject,
                        modifier = Modifier.weight(1f)
                    ) {
                        Text("Decline")
                    }

                    Button(
                        onClick = onAccept,
                        modifier = Modifier.weight(1f),
                        colors = ButtonDefaults.buttonColors(
                            containerColor = MaterialTheme.colorScheme.primary
                        )
                    ) {
                        Text("Accept")
                    }
                }
            }
        }
    }
}


@androidx.annotation.OptIn(ExperimentalGetImage::class)
@Composable
private fun ColumnScope.ScanQRButton(modifier: Modifier = Modifier) {
    val context = LocalContext.current

    Button(
        onClick = {
            context.startActivity(Intent(context, QrScanner::class.java))
        },
        modifier = modifier
            .fillMaxWidth(0.8f)
            .align(Alignment.CenterHorizontally),
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Icon(
                painter = painterResource(R.drawable.i_qr_code_scanner),
                contentDescription = "QR Code Scanner Icon"
            )

            Text(
                "Scan QR Code",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelLargeEmphasized
            )
        }
    }
}
