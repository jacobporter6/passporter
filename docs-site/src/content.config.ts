import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { docsSchema } from '@astrojs/starlight/schema';
import { portableLoader } from './lib/portable-loader';

// Extra frontmatter the handbook understands (all optional):
//   pending: "PR #91"   -> banner: documents code not yet merged to main
//   pending: "PR #90 (push rows)" -> "Partly ahead of main" banner naming the part
//   pending_scope: text -> same partial banner, scope given separately
//   sources: [paths]    -> "Source of truth" chips under the title
//   status: generated   -> banner: generated page, edit the generator
export const collections = {
  docs: defineCollection({
    loader: portableLoader(),
    schema: docsSchema({
      extend: z.object({
        pending: z.string().optional(),
        pending_scope: z.string().optional(),
        sources: z.array(z.string()).optional(),
        status: z.enum(['generated', 'draft', 'stable']).optional(),
      }),
    }),
  }),
};
