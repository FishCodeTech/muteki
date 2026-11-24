type Tone = "default" | "accent" | "warning" | "danger" | "success";

function categoryTone(category: string): Tone {
  const cat = category.toLowerCase().trim();
  if (["web", "websec", "api", "cloud"].includes(cat)) return "accent";
  if (["pwn", "binary", "exp", "exploit", "rev", "reverse"].includes(cat)) return "danger";
  if (["crypto", "cryptography", "math", "algo"].includes(cat)) return "warning";
  if (["forensics", "stego", "defense", "incident"].includes(cat)) return "success";
  if (["misc", "ai", "hardware", "iot", "blockchain", "pentest"].includes(cat)) return "accent";
  return "default";
}

export function RunCategoryTag({ category }: { category?: string }) {
  if (!category) return null;
  return <span className="rv-cat" data-tone={categoryTone(category)}>{category}</span>;
}
