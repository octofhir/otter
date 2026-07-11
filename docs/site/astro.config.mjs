// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
	site: 'https://octofhir.github.io',
	base: '/otter',
	integrations: [
		starlight({
			title: 'Otter Engine',
			description:
				'Contributor guide and conformance dashboard for the Otter JavaScript engine.',
			social: [
				{
					icon: 'github',
					label: 'GitHub',
					href: 'https://github.com/octofhir/otter',
				},
			],
			editLink: {
				baseUrl: 'https://github.com/octofhir/otter/edit/main/docs/site/',
			},
			sidebar: [
				{ label: 'Introduction', slug: 'introduction' },
				{
					label: 'Contributing',
					items: [
						{ label: 'Overview', slug: 'contributing/overview' },
						{ label: 'Dependency Policy', slug: 'contributing/dependency-policy' },
						{ label: 'Porting Process', slug: 'contributing/porting' },
						{ label: 'Adding a Built-in Method', slug: 'contributing/builtin-methods' },
						{ label: 'Test Harness', slug: 'contributing/test-harness' },
					],
				},
				{
					label: 'Engine Internals',
					items: [
						{ label: 'Architecture', slug: 'engine/architecture' },
						{ label: 'Runtime Principles', slug: 'engine/runtime-principles' },
						{ label: 'Event Loop', slug: 'engine/event-loop' },
						{ label: 'Workers', slug: 'engine/workers' },
						{ label: 'Frontend And Compilation', slug: 'engine/frontend' },
						{ label: 'GC API', slug: 'engine/gc-api' },
						{ label: 'Native Call ABI', slug: 'engine/native-call-abi' },
						{ label: 'Step Trace', slug: 'engine/step-trace' },
					],
				},
				{
					label: 'Conformance',
					items: [{ label: 'ECMAScript Conformance', slug: 'conformance' }],
				},
				{
					label: 'Package Manager',
					items: [{ label: 'Development Loop', slug: 'package-manager/development-loop' }],
				},
				{
					label: 'Extensions And Plugins',
					items: [
						{ label: 'Overview', slug: 'extensions/overview' },
						{
							label: 'Declarative Bindings: Classes, Namespaces, Extensions',
							slug: 'extensions/declarative-bindings',
						},
						{
							label: 'Embedding: Writing an Extension',
							slug: 'extensions/embedding-extensions',
						},
						{ label: 'Hosted Modules', slug: 'extensions/hosted-modules' },
						{ label: 'Native Bindings', slug: 'extensions/native-bindings' },
						{ label: 'Handle Scopes: Building JS Values', slug: 'extensions/handle-scopes' },
						{ label: 'JS Surface Builders', slug: 'extensions/js-surface-builders' },
						{ label: 'Plugin System', slug: 'extensions/plugin-system' },
					],
				},
				{
					label: 'Web APIs',
					items: [
						{ label: 'Contribution Workflow', slug: 'web/contributing' },
						{ label: 'WebAssembly: Status & Roadmap', slug: 'web/webassembly-roadmap' },
					],
				},
				{
					label: 'Performance',
					items: [{ label: 'Startup', slug: 'performance/startup' }],
				},
				{
					label: 'Deployment',
					items: [{ label: 'macOS', slug: 'deployment/macos' }],
				},
				{
					label: 'Macros',
					items: [
						{ label: 'Overview', slug: 'macros/overview' },
						{ label: 'Design Note', slug: 'macros/design' },
					],
				},
			],
		}),
	],
});
