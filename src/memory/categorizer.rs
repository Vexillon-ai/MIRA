// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/categorizer.rs

//! Auto-categorization logic for memories
//! 
//! Uses simple keyword matching and heuristics to automatically categorize
//! user input into appropriate memory categories.

use super::Category;
use tracing::debug;

/// Simple auto-categorizer using keyword matching
pub struct MemoryCategorizer;

impl MemoryCategorizer {
    /// Create a new categorizer
    pub fn new() -> Self {
        Self
    }
    
    /// Categorize text based on content analysis
    pub fn categorize(&self, text: &str) -> Category {
        let lower = text.to_lowercase();
        
        // Check for preference indicators first (high priority)
        if self.is_preference(&lower) {
            return Category::Preference;
        }
        
        // Check for skill/ability indicators
        if self.is_skill(&lower) {
            return Category::Skill;
        }
        
        // Check for relationship indicators
        if self.is_relationship(&lower) {
            return Category::Relationship;
        }
        
        // Check for project/goal indicators
        if self.is_project(&lower) {
            return Category::Project;
        }
        
        // Default to fact
        Category::Fact
    }
    
    /// Detect preferences: likes, dislikes, wants, prefers, etc.
    fn is_preference(&self, text: &str) -> bool {
        let preference_keywords = [
            "like", "love", "hate", "dislike", "prefer",
            "want", "would like", "enjoy", "appreciate",
            "don't like", "can't stand", "really like",
            "favorite", "best", "worst",
        ];
        
        for keyword in &preference_keywords {
            if text.contains(keyword) {
                debug!("Detected preference: '{}' contains '{}'", text, keyword);
                return true;
            }
        }
        false
    }
    
    /// Detect skills: know how to, able to, good at, etc.
    fn is_skill(&self, text: &str) -> bool {
        // Note: "can" alone is intentionally excluded — it matches too broadly
        // (e.g. "I can see you tomorrow", "Canada"). Use "i can " with a trailing
        // space so it only matches first-person skill statements.
        let skill_keywords = [
            "i can ", "know how to", "able to", "good at",
            "expert in", "skilled in", "proficient in",
            "i know how", "i learned", "studied", "certified in",
            "i'm good at",
        ];
        
        for keyword in &skill_keywords {
            if text.contains(keyword) {
                debug!("Detected skill: '{}' contains '{}'", text, keyword);
                return true;
            }
        }
        false
    }
    
    /// Detect relationships: friend, family, colleague, etc.
    fn is_relationship(&self, text: &str) -> bool {
        let relationship_keywords = [
            "friend", "family", "colleague", "coworker",
            "partner", "spouse", "wife", "husband",
            "mother", "father", "son", "daughter",
            "brother", "sister", "uncle", "aunt",
            "neighbor", "boss", "employee",
        ];
        
        for keyword in &relationship_keywords {
            if text.contains(keyword) {
                debug!("Detected relationship: '{}' contains '{}'", text, keyword);
                return true;
            }
        }
        false
    }
    
    /// Detect projects/goals: working on, planning to, goal is, etc.
    fn is_project(&self, text: &str) -> bool {
        let project_keywords = [
            "working on", "building", "creating",
            "planning to", "goal is", "trying to",
            "project", "currently working", "started",
            "learning how to", "want to learn",
        ];
        
        for keyword in &project_keywords {
            if text.contains(keyword) {
                debug!("Detected project: '{}' contains '{}'", text, keyword);
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_preference_detection() {
        let categorizer = MemoryCategorizer::new();
        assert_eq!(categorizer.categorize("I like pizza"), Category::Preference);
        assert_eq!(categorizer.categorize("I love Rust programming"), Category::Preference);
        assert_eq!(categorizer.categorize("My favorite color is blue"), Category::Preference);
    }
    
    #[test]
    fn test_skill_detection() {
        let categorizer = MemoryCategorizer::new();
        assert_eq!(categorizer.categorize("I can speak French"), Category::Skill);
        assert_eq!(categorizer.categorize("I'm good at coding"), Category::Skill);
    }
    
    #[test]
    fn test_relationship_detection() {
        let categorizer = MemoryCategorizer::new();
        assert_eq!(categorizer.categorize("My friend Sarah lives in Paris"), Category::Relationship);
        assert_eq!(categorizer.categorize("I have two brothers"), Category::Relationship);
    }
    
    #[test]
    fn test_project_detection() {
        let categorizer = MemoryCategorizer::new();
        assert_eq!(categorizer.categorize("I'm working on a web app"), Category::Project);
        assert_eq!(categorizer.categorize("My goal is to learn AI"), Category::Project);
    }
    
    #[test]
    fn test_fact_detection() {
        let categorizer = MemoryCategorizer::new();
        // Facts don't match any special patterns
        assert_eq!(categorizer.categorize("My name is Tarek"), Category::Fact);
        assert_eq!(categorizer.categorize("I live in Cairo"), Category::Fact);
    }
}
