import type { MetadataRoute } from "next";

export default function manifest(): MetadataRoute.Manifest {
  return {
    name: "Clipboard Vault",
    short_name: "Clip Vault",
    description: "Private, searchable memory for everything you copy.",
    start_url: "/",
    display: "standalone",
    background_color: "#0d0f0e",
    theme_color: "#0d0f0e",
    icons: [{ src: "/favicon.svg", sizes: "any", type: "image/svg+xml" }],
  };
}
