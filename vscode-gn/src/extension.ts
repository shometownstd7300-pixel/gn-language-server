/**
 * Copyright 2025 Google LLC
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

import * as path from 'path';
import * as vscode from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions, TransportKind } from 'vscode-languageclient/node';

const EXECUTABLE_SUFFIX: string = process.platform === 'win32' ? '.exe' : '';

export async function activate(context: vscode.ExtensionContext): Promise<void> {
	const output = vscode.window.createOutputChannel('GN');

	const clientOptions: LanguageClientOptions = {
		documentSelector: [
			{'scheme': 'file', 'pattern': '**/BUILD.gn'},
			{'scheme': 'file', 'pattern': '**/*.gni'},
			{'scheme': 'file', 'pattern': '**/.gn'},
		],
		synchronize: {
			fileEvents: [
				vscode.workspace.createFileSystemWatcher('**/BUILD.gn'),
				vscode.workspace.createFileSystemWatcher('**/*.gni'),
				vscode.workspace.createFileSystemWatcher('**/.gn'),
			],
		},
		outputChannel: output,
	};

	const extensionDir = context.extensionPath;
	const serverOptions: ServerOptions = {
		transport: TransportKind.stdio,
		command: path.join(extensionDir, 'dist/gn-language-server' + EXECUTABLE_SUFFIX),
		options: {
			cwd: extensionDir,
			env: {
				RUST_BACKTRACE: '1',
			},
		},
	};

	const client = new LanguageClient(
		'gn',
		'GN',
		serverOptions,
		clientOptions
	);
	context.subscriptions.push(client);

	await client.start();
}

export async function deactivate(): Promise<void> {
}
